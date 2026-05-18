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
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState},
};

use crate::client::{BreakerRow, EventRow, SessionRow};

/// One-line key hint shown in the status bar before any action is taken.
pub const KEY_HINT: &str = "keys: ↑↓ navigate | ↵ focus | p pause | r resume | x stop | o iTerm2 tab | : / command | Esc back | q quit";

/// Maximum number of executed commands kept in the command-bar history.
pub const COMMAND_HISTORY_LIMIT: usize = 20;

/// Every slash command the command bar knows, in autocomplete order.
///
/// Why: the command bar's Tab autocomplete and `/help` both enumerate the known
/// commands; keeping the canonical list here means a new command is registered
/// in exactly one place.
/// What: the bare command verbs, each as it would appear after the leading `/`.
/// Test: `known_commands_contains_core_verbs`.
pub const KNOWN_COMMANDS: &[&str] = &[
    "pair", "projects", "sessions", "tmux", "status", "adopt", "connect", "chat", "send",
    "discover", "help", "exit", "quit",
];

/// The persistent slash-command bar pinned to the bottom of the dashboard.
///
/// Why: the operator needs a vim-style command line that is always visible — an
/// output panel showing the last command's result above a single input line —
/// rather than transient modal overlays. Holding all of its mutable state in one
/// struct keeps the event loop and the renderer in sync.
/// What: `active` gates whether keystrokes are captured for editing; `input` is
/// the current edit buffer; `output` holds the last result's wrapped lines;
/// `history` is a ring of the last [`COMMAND_HISTORY_LIMIT`] executed commands;
/// `history_cursor` tracks ↑/↓ recall; `autocomplete_cycle` tracks Tab cycling.
/// Test: `command_bar_*` unit tests cover activation, editing, history, and
/// autocomplete.
#[derive(Debug, Clone, Default)]
pub struct CommandBar {
    /// Whether the bar is capturing keystrokes (cursor shown, input editable).
    pub active: bool,
    /// The current edit buffer (without the leading `/`).
    pub input: String,
    /// Wrapped output lines from the last executed command.
    pub output: Vec<String>,
    /// Recently executed commands, newest last, capped at the history limit.
    pub history: Vec<String>,
    /// Index into [`Self::history`] while recalling with ↑/↓; `None` = live input.
    history_cursor: Option<usize>,
    /// Tab autocomplete state: `(matches, index)` for the current cycle.
    autocomplete_cycle: Option<(Vec<String>, usize)>,
}

impl CommandBar {
    /// Activate the command bar so it begins capturing keystrokes.
    ///
    /// Why: the `:` and `/` keys turn the always-visible bar into an editable
    /// command line; the activating key itself is consumed, not buffered, so the
    /// operator types the bare command verb.
    /// What: sets [`Self::active`] and resets the history cursor and the
    /// autocomplete cycle so a fresh edit session starts clean.
    /// Test: `command_bar_activate_deactivate`.
    pub fn activate(&mut self) {
        self.active = true;
        self.history_cursor = None;
        self.autocomplete_cycle = None;
    }

    /// Deactivate the command bar and clear the edit buffer (the Esc key).
    ///
    /// Why: Esc must abandon a half-typed command and return single-key
    /// shortcuts to normal operation; the output panel is left intact so the
    /// operator can still read the last result.
    /// What: clears [`Self::active`] and [`Self::input`], resets recall state.
    /// Test: `command_bar_activate_deactivate`.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.input.clear();
        self.history_cursor = None;
        self.autocomplete_cycle = None;
    }

    /// Append a character to the input buffer while the bar is active.
    ///
    /// Why: printable keystrokes build up the command string.
    /// What: pushes `c`; resets the autocomplete cycle and history cursor so the
    /// next Tab / ↑ starts fresh. A no-op when the bar is inactive.
    /// Test: `command_bar_edits_buffer`.
    pub fn push(&mut self, c: char) {
        if !self.active {
            return;
        }
        self.input.push(c);
        self.autocomplete_cycle = None;
        self.history_cursor = None;
    }

    /// Delete the last character of the input buffer (the Backspace key).
    ///
    /// Why: Backspace must edit a mistyped command.
    /// What: pops the trailing character; resets autocomplete / recall state.
    /// A no-op when the bar is inactive or the buffer is empty.
    /// Test: `command_bar_edits_buffer`.
    pub fn backspace(&mut self) {
        if !self.active {
            return;
        }
        self.input.pop();
        self.autocomplete_cycle = None;
        self.history_cursor = None;
    }

    /// Cycle Tab autocomplete through the commands matching the current prefix.
    ///
    /// Why: pressing Tab on `/p` should cycle `/pair`, `/projects` so the
    /// operator never has to type a full command name.
    /// What: on the first Tab for a prefix, collects every [`KNOWN_COMMANDS`]
    /// entry that starts with the typed verb and replaces the buffer with the
    /// first match; each further Tab advances to the next match, wrapping. A
    /// no-op when the bar is inactive or nothing matches.
    /// Test: `command_bar_tab_cycles_matches`.
    pub fn autocomplete(&mut self) {
        if !self.active {
            return;
        }
        // Continue an in-progress cycle when the buffer still matches it.
        if let Some((matches, idx)) = self.autocomplete_cycle.as_mut()
            && self.input == matches[*idx]
        {
            *idx = (*idx + 1) % matches.len();
            self.input = matches[*idx].clone();
            return;
        }
        let prefix = normalize_command(&self.input);
        let matches: Vec<String> = KNOWN_COMMANDS
            .iter()
            .filter(|cmd| cmd.starts_with(&prefix))
            .map(|cmd| (*cmd).to_string())
            .collect();
        if matches.is_empty() {
            self.autocomplete_cycle = None;
            return;
        }
        self.input = matches[0].clone();
        self.autocomplete_cycle = Some((matches, 0));
    }

    /// Recall the previous command from history (the ↑ key).
    ///
    /// Why: re-running or editing a recent command should not require retyping.
    /// What: moves the history cursor one step toward the oldest entry and loads
    /// it into the buffer; a no-op when the bar is inactive or history is empty.
    /// Test: `command_bar_history_recall`.
    pub fn history_prev(&mut self) {
        if !self.active || self.history.is_empty() {
            return;
        }
        let next = match self.history_cursor {
            None => self.history.len() - 1,
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_cursor = Some(next);
        self.input = self.history[next].clone();
        self.autocomplete_cycle = None;
    }

    /// Recall the next (newer) command from history (the ↓ key).
    ///
    /// Why: lets the operator step back down after recalling too far with ↑.
    /// What: advances the history cursor toward the newest entry; stepping past
    /// the newest clears the cursor and empties the buffer (back to live input).
    /// A no-op when the bar is inactive or no recall is in progress.
    /// Test: `command_bar_history_recall`.
    pub fn history_next(&mut self) {
        if !self.active {
            return;
        }
        let Some(i) = self.history_cursor else {
            return;
        };
        if i + 1 >= self.history.len() {
            self.history_cursor = None;
            self.input.clear();
        } else {
            self.history_cursor = Some(i + 1);
            self.input = self.history[i + 1].clone();
        }
        self.autocomplete_cycle = None;
    }

    /// Take the typed command for execution, recording it in history.
    ///
    /// Why: pressing Enter dispatches the command; the loop needs the buffer's
    /// text and the bar needs the command pushed onto its bounded history.
    /// What: returns the trimmed buffer, clears the input, appends a non-empty
    /// command to [`Self::history`] (dropping the oldest beyond the limit), and
    /// resets the recall / autocomplete state. The bar stays active.
    /// Test: `command_bar_submit_records_history`.
    pub fn take_for_execution(&mut self) -> String {
        let typed = std::mem::take(&mut self.input);
        self.history_cursor = None;
        self.autocomplete_cycle = None;
        let trimmed = typed.trim().to_string();
        if !trimmed.is_empty() {
            self.history.push(trimmed.clone());
            if self.history.len() > COMMAND_HISTORY_LIMIT {
                let overflow = self.history.len() - COMMAND_HISTORY_LIMIT;
                self.history.drain(0..overflow);
            }
        }
        trimmed
    }

    /// Replace the output panel with the result of the last command.
    ///
    /// Why: command results are shown inline in the panel above the input line
    /// rather than in a modal popup.
    /// What: stores `lines` as the new output, discarding the previous result.
    /// Test: `command_bar_set_output`.
    pub fn set_output(&mut self, lines: Vec<String>) {
        self.output = lines;
    }
}

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
    /// The persistent vim-style slash-command bar pinned to the bottom.
    ///
    /// Why: the operator drives every slash command (`/pair`, `/projects`, …)
    /// from one always-visible bar with an inline output panel, Tab
    /// autocomplete, and ↑/↓ history — replacing the old transient prompts and
    /// the pairing modal popup.
    /// What: see [`CommandBar`]; `command_bar.active` gates whether keystrokes
    /// are captured for editing or fall through to single-key shortcuts.
    /// Test: `command_bar_*` unit tests.
    pub command_bar: CommandBar,
    /// Rolling LLM chat history for the `/chat` command.
    ///
    /// Why: the `/chat` command drives the daemon's stateless `POST /llm/chat`
    /// endpoint; the TUI holds the conversation window so successive `/chat`
    /// calls form one continuous conversation.
    /// What: the message history, updated after every successful `/chat` turn.
    /// Test: `dispatch_chat_writes_error_when_daemon_down`.
    pub chat_history: Vec<trusty_mpm_client::ChatMessage>,
    /// The session the command bar's summarized-chat mode targets.
    ///
    /// Why: plain (non-`/`) text typed in the CMD> bar is sent to a "focused"
    /// session and its output summarized; the bar must remember which session
    /// that is. Pressing Enter on a highlighted session row sets it.
    /// What: the focused session's friendly tmux name (or UUID string when it
    /// has no name); `None` when no session is focused.
    /// Test: `set_active_session_uses_selected_target`,
    /// `command_input_line_shows_focused_session`.
    pub active_session: Option<String>,
    /// Captured output for the focused session's detail panel.
    ///
    /// Why: selecting a session opens a "Session Output / History" panel beside
    /// the session list; the event loop refreshes this every poll with a tmux
    /// pane snapshot (tmux-origin sessions) or the session's recent hook events
    /// (native-origin sessions).
    /// What: the panel's text lines, newest content last; empty when no session
    /// is focused or the daemon returned nothing.
    /// Test: `session_output_panel_lines_*` unit tests.
    pub session_output: Vec<String>,
    /// Whether the event loop should exit (set by `/exit` or `/quit`).
    ///
    /// Why: `/exit` and `/quit` must leave the dashboard exactly like the `q`
    /// key; the command dispatcher cannot return from the loop itself, so it
    /// raises this flag and the loop checks it after dispatch.
    /// What: `false` until an `/exit` / `/quit` command runs.
    /// Test: `dispatch_exit_sets_should_exit`.
    pub should_exit: bool,
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
        if let Some(idx) = self.sessions.iter().position(|s| s.id.0.to_string() == id) {
            self.selected_session = idx;
            true
        } else {
            false
        }
    }

    /// Mark the currently-highlighted session as the command-bar's target.
    ///
    /// Why: pressing Enter on a session row "focuses" it so plain text typed in
    /// the CMD> bar is routed to that session's summarized-chat mode and the
    /// session-output detail panel opens beside the list.
    /// What: copies [`Self::selected_target`] into [`Self::active_session`] and
    /// clears any stale [`Self::session_output`] so the panel repaints on the
    /// next poll; returns the focused name, or `None` when the list is empty.
    /// Test: `set_active_session_uses_selected_target`.
    pub fn set_active_session(&mut self) -> Option<String> {
        let target = self.selected_target().filter(|t| !t.is_empty());
        self.active_session = target.clone();
        self.session_output.clear();
        target
    }

    /// Deselect the focused session, closing the detail panel.
    ///
    /// Why: pressing Esc while the command bar is inactive must drop the
    /// focused session so the top area returns to the full session list.
    /// What: clears [`Self::active_session`] and [`Self::session_output`];
    /// returns `true` when a session had been focused.
    /// Test: `clear_active_session_drops_focus`.
    pub fn clear_active_session(&mut self) -> bool {
        let had = self.active_session.is_some();
        self.active_session = None;
        self.session_output.clear();
        had
    }

    /// Build the [`SessionSummary`] slice the resolver searches.
    ///
    /// Why: `trusty_mpm_core::resolve_target` works on its own minimal summary
    /// type; the dashboard's `SessionRow` carries extra render-only fields, so a
    /// projection is needed before resolution.
    /// What: maps each polled `SessionRow` to a `SessionSummary` — UUID string,
    /// friendly `tmux_name`, `workdir`, and `last_seen` (as Unix seconds) so
    /// workdir-prefix recency tie-breaking in `resolve_target` picks the most
    /// recently active session.
    /// Test: covered indirectly by `submit_connect_*` tests.
    fn session_summaries(&self) -> Vec<trusty_mpm_core::SessionSummary> {
        self.sessions
            .iter()
            .map(|s| trusty_mpm_core::SessionSummary {
                id: s.id.0.to_string(),
                name: Some(s.tmux_name.clone()).filter(|n| !n.is_empty()),
                workdir: s.workdir.clone(),
                last_active: s.last_seen.secs_since_epoch,
            })
            .collect()
    }

    /// Resolve a fuzzy target and focus the matching session for `/connect`.
    ///
    /// Why: the `/connect <id>` command jumps the dashboard selection to a
    /// session resolved from a fuzzy target — id prefix, name, or workdir.
    /// What: resolves `target` against the current sessions via
    /// [`trusty_mpm_core::resolve_target`]; on `Found` it focuses the row and
    /// returns `"Connected to <id>"`, on `Ambiguous`/`NotFound` it returns the
    /// matching status line. Does not touch any input buffer.
    /// Test: `resolve_connect_found`, `resolve_connect_not_found`,
    /// `resolve_connect_ambiguous`.
    pub fn resolve_connect(&mut self, target: &str) -> String {
        match self.connect_action(target) {
            ConnectAction::Resolved(msg) => msg,
            // A launch target with no matching session can't be handled
            // synchronously — callers that can't launch report the directory.
            ConnectAction::Launch(dir) => format!("No session for {dir}"),
        }
    }

    /// Classify a `/connect <target>` into a synchronous result or a launch.
    ///
    /// Why: `/connect` is the single entry point for "connect to or launch a
    /// session for a project". A plain id/name resolves immediately, but a
    /// directory target with no existing session needs an async daemon launch
    /// that `DashboardState` (sync, no client) cannot perform itself.
    /// What: if `target` looks like a directory (see [`looks_like_dir`]), it is
    /// expanded (`~` → `$HOME`) and matched against existing session workdirs;
    /// a match focuses that session, a miss returns [`ConnectAction::Launch`]
    /// with the expanded directory. Non-directory targets fall back to the
    /// fuzzy id/name resolver and always return [`ConnectAction::Resolved`].
    /// Test: `connect_action_focuses_session_with_matching_workdir`,
    /// `connect_action_routes_unmatched_dir_to_launch`,
    /// `connect_action_resolves_fuzzy_name`.
    pub fn connect_action(&mut self, target: &str) -> ConnectAction {
        let trimmed = target.trim();
        if looks_like_dir(trimmed) {
            let dir = expand_dir(trimmed);
            // A directory target first tries an existing session by workdir;
            // canonicalize both sides so `/p/a` and `/p/a/` compare equal.
            let normalized = normalize_workdir(&dir);
            if let Some(row) = self
                .sessions
                .iter()
                .find(|s| normalize_workdir(&s.workdir) == normalized)
            {
                let id = row.id.0.to_string();
                self.focus_on(&id);
                return ConnectAction::Resolved(format!("Connected to {id}"));
            }
            return ConnectAction::Launch(dir);
        }

        let summaries = self.session_summaries();
        let msg = match trusty_mpm_core::resolve_target(trimmed, &summaries) {
            trusty_mpm_core::ResolveResult::Found(id) => {
                self.focus_on(&id);
                format!("Connected to {id}")
            }
            trusty_mpm_core::ResolveResult::Ambiguous(ids) => {
                format!("Ambiguous: {}", ids.join(", "))
            }
            trusty_mpm_core::ResolveResult::NotFound => "No session matched".to_string(),
        };
        ConnectAction::Resolved(msg)
    }
}

/// Outcome of classifying a `/connect <target>` argument.
///
/// Why: directory targets without an existing session require an async daemon
/// launch the sync `DashboardState` cannot perform; the dispatcher needs an
/// explicit signal to either show a message or run the launch.
/// What: `Resolved` carries a finished status line; `Launch` carries the
/// expanded directory the caller should start a Claude Code session in.
/// Test: `connect_action_*` tests in this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectAction {
    /// The command completed synchronously; the string is the status line.
    Resolved(String),
    /// No session matched the directory — launch a new one here.
    Launch(String),
}

/// Decide whether a `/connect` argument is a directory path.
///
/// Why: `/connect` accepts both session ids/names and directory paths; only
/// paths route to the launch flow. A simple prefix check disambiguates them.
/// What: returns `true` when the trimmed target starts with `/` (absolute) or
/// `~` (home-relative).
/// Test: `looks_like_dir_detects_paths`.
fn looks_like_dir(target: &str) -> bool {
    let t = target.trim();
    t.starts_with('/') || t.starts_with('~')
}

/// Expand a `~`-prefixed path against `$HOME`.
///
/// Why: operators type `~/project`; the daemon and tmux need an absolute path.
/// What: replaces a leading `~` with `$HOME` when set; otherwise returns the
/// input unchanged. Absolute paths pass through untouched.
/// Test: `expand_dir_expands_tilde`.
fn expand_dir(target: &str) -> String {
    let t = target.trim();
    if let Some(rest) = t.strip_prefix('~')
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{rest}");
    }
    t.to_string()
}

/// Normalize a workdir string for equality comparison.
///
/// Why: a session's stored workdir may or may not carry a trailing slash; the
/// `/connect <dir>` lookup must treat `/p/a` and `/p/a/` as the same project.
/// What: trims whitespace and a single trailing `/` (but never the root `/`).
/// Test: `normalize_workdir_strips_trailing_slash`.
fn normalize_workdir(dir: &str) -> &str {
    let t = dir.trim();
    match t.strip_suffix('/') {
        Some(stripped) if !stripped.is_empty() => stripped,
        _ => t,
    }
}

/// Canonicalize a typed slash-command into its bare verb.
///
/// Why: the `command>` prompt accepts `/pair`, `pair`, or `  /Pair ` — all the
/// same intent — so dispatch logic needs one normalized form.
/// What: trims surrounding whitespace, strips a leading `/`, and lowercases the
/// result.
/// Test: `normalize_command_strips_slash_and_case`.
pub fn normalize_command(input: &str) -> String {
    input.trim().trim_start_matches('/').trim().to_lowercase()
}

/// Pick the display colour for a session status string.
///
/// Why: a colour-coded status cell makes the operator's eye jump to trouble —
/// centralising the mapping keeps `session_rows` readable and unit-testable.
/// What: `"active"` → green, `"paused"` → yellow, anything else → white. The
/// fallback is white (not `Gray`) because `Gray` is a dim mid-tone that is
/// nearly invisible against the dashboard background on many terminal themes.
/// Test: `session_status_colours`.
fn session_status_color(status: &str) -> Color {
    match status {
        "active" => Color::Green,
        "paused" => Color::Yellow,
        _ => Color::White,
    }
}

/// Render a [`SessionStatus`] as a lowercase display label.
///
/// Why: the session table shows a short status word and colours it via
/// [`session_status_color`], which keys on lowercase names.
/// What: maps each status variant to its lowercase name.
/// Test: `session_rows_format_each_session`.
fn status_label(status: trusty_mpm_core::session::SessionStatus) -> &'static str {
    use trusty_mpm_core::session::SessionStatus;
    match status {
        SessionStatus::Starting => "starting",
        SessionStatus::Active => "active",
        SessionStatus::AwaitingApproval => "awaiting_approval",
        SessionStatus::Detached => "detached",
        SessionStatus::Paused => "paused",
        SessionStatus::Stopped => "stopped",
    }
}

/// Pick the display colour for a circuit-breaker state string.
///
/// Why: an at-a-glance colour for breaker state surfaces open breakers
/// immediately; centralising the mapping keeps `breaker_rows` testable.
/// What: `"closed"` → green, `"half_open"` → yellow, `"open"` → red, anything
/// else → white. The fallback is white (not `Gray`) so an unrecognised state
/// stays readable instead of fading into a dim mid-tone.
/// Test: `breaker_state_colours`.
fn breaker_state_color(state: &str) -> Color {
    match state {
        "closed" => Color::Green,
        "half_open" => Color::Yellow,
        "open" => Color::Red,
        _ => Color::White,
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
/// is factored here where a test can assert it directly. The selected row must
/// stand out unambiguously; a `DarkGray` background (the previous choice) is a
/// dim mid-tone that barely separated the highlighted row from the body on
/// many terminal themes.
/// What: a solid `Blue` background with bold `White` foreground for the
/// selected row — a high-contrast pairing legible on both dark and light
/// terminals — and the default (reset) style otherwise.
/// Test: `selected_row_is_highlighted`.
pub fn session_row_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
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
            let id = s.id.0.to_string().chars().take(8).collect::<String>();
            let status = status_label(s.status);
            let status_color = session_status_color(status);
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

/// Render a [`SessionId`] into a short, human id.
///
/// Why: the dashboard shows only the first 8 characters of a session UUID so
/// rows and event lines stay compact.
/// What: truncates the UUID string to its first 8 characters.
/// Test: `short_session_extracts_prefix`.
pub(crate) fn short_session(id: &trusty_mpm_core::session::SessionId) -> String {
    id.0.to_string().chars().take(8).collect()
}

/// Extract the most useful human-readable detail from a hook event payload.
///
/// Why: a bare `FileChanged` row tells the operator nothing — the changed file,
/// the tool name, or the session name is what they actually need to see.
/// What: keys on the [`HookEvent`] variant to pull the relevant field from the
/// opaque payload — `FileChanged` → the path's basename, tool events → the
/// `tool` name, session events → a session name, otherwise the first useful
/// string field — returning an empty string when nothing meaningful is present.
/// Test: `event_detail_*` unit tests cover each branch.
pub fn event_detail(
    event: trusty_mpm_core::hook::HookEvent,
    payload: &serde_json::Value,
) -> String {
    use trusty_mpm_core::hook::HookEvent;

    /// Pull a string field from the payload, returning "" when absent.
    fn field<'a>(payload: &'a serde_json::Value, key: &str) -> &'a str {
        payload.get(key).and_then(|v| v.as_str()).unwrap_or("")
    }

    match event {
        HookEvent::FileChanged => {
            let path = field(payload, "path");
            std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path)
                .to_string()
        }
        HookEvent::PreToolUse | HookEvent::PostToolUse | HookEvent::PostToolUseFailure => {
            field(payload, "tool").to_string()
        }
        HookEvent::SessionStart | HookEvent::SessionEnd => {
            let name = field(payload, "session_name");
            if name.is_empty() {
                field(payload, "name").to_string()
            } else {
                name.to_string()
            }
        }
        HookEvent::SubagentStart | HookEvent::SubagentStop | HookEvent::SubagentStopFailure => {
            field(payload, "agent").to_string()
        }
        HookEvent::SkillActivated => field(payload, "skill").to_string(),
        HookEvent::WorktreeCreate | HookEvent::WorktreeRemove => field(payload, "path").to_string(),
        HookEvent::UserPromptSubmit => {
            let prompt = field(payload, "prompt");
            prompt.chars().take(40).collect()
        }
        _ => {
            // Fall back to the first useful string field commonly present.
            for key in ["message", "tool", "path", "name", "reason"] {
                let v = field(payload, key);
                if !v.is_empty() {
                    return v.chars().take(40).collect();
                }
            }
            String::new()
        }
    }
}

/// Build the formatted lines for the recent-events panel.
///
/// Why: separating line formatting from the ratatui `List` lets tests assert
/// the text without a terminal backend.
/// What: the last 20 events, each as
/// `{event:<22} {detail:<20} {session_short:<10} {at}` where `detail` is the
/// per-event detail from [`event_detail`] (filename, tool name, …).
/// Test: `event_lines_format_recent_events`.
pub fn event_lines(state: &DashboardState) -> Vec<String> {
    let start = state.events.len().saturating_sub(20);
    state.events[start..]
        .iter()
        .map(|e| {
            let session = short_session(&e.session);
            let detail = event_detail(e.event, &e.payload);
            format!(
                "{:<22} {:<20} {:<10} {}",
                e.event.wire_name(),
                detail,
                session,
                e.at
            )
        })
        .collect()
}

/// Maximum number of lines kept in the session-output detail panel.
pub const SESSION_OUTPUT_LIMIT: usize = 50;

/// Build the lines shown in the session-output detail panel.
///
/// Why: the panel content comes from one of two sources depending on the
/// session's origin — the event loop fills [`DashboardState::session_output`]
/// with a tmux snapshot for tmux-origin sessions and falls back here to the
/// session's recent hook events; keeping the fallback formatting testable means
/// it can be asserted without a terminal.
/// What: returns the trailing [`SESSION_OUTPUT_LIMIT`] lines of
/// `session_output` when it is non-empty; otherwise formats the events whose
/// session matches `active_session` (resolved by friendly name or UUID prefix)
/// as `{event:<22} {detail}` lines; a placeholder when nothing is available.
/// Test: `session_output_panel_lines_*`.
pub fn session_output_panel_lines(state: &DashboardState) -> Vec<String> {
    if !state.session_output.is_empty() {
        let start = state
            .session_output
            .len()
            .saturating_sub(SESSION_OUTPUT_LIMIT);
        return state.session_output[start..].to_vec();
    }
    // Fall back to this session's recent hook events.
    let Some(focused) = state.active_session.as_deref() else {
        return vec!["(no session focused)".to_string()];
    };
    let session_id = state
        .sessions
        .iter()
        .find(|s| s.tmux_name == focused || s.id.0.to_string() == focused)
        .map(|s| s.id);
    let matching: Vec<String> = state
        .events
        .iter()
        .filter(|e| match session_id {
            Some(id) => e.session == id,
            None => true,
        })
        .map(|e| {
            let detail = event_detail(e.event, &e.payload);
            format!("{:<22} {detail}", e.event.wire_name())
        })
        .collect();
    if matching.is_empty() {
        vec![format!("(no recent output for {focused})")]
    } else {
        let start = matching.len().saturating_sub(SESSION_OUTPUT_LIMIT);
        matching[start..].to_vec()
    }
}

/// Pick the header-title colour from the daemon's reachability.
///
/// Why: the title doubles as a health indicator — the operator should see a
/// problem from the colour alone. Factoring the choice out of `render` lets a
/// test assert the error colour without a terminal frame.
/// What: `Cyan` when the daemon is reachable, `Red` when it is not.
/// Test: `title_color_signals_daemon_health`.
fn title_color(daemon_reachable: bool) -> Color {
    if daemon_reachable {
        Color::Cyan
    } else {
        Color::Red
    }
}

/// Build the header-title style from the daemon's reachability.
///
/// Why: a bare red foreground is easy to miss on a busy or light-themed
/// terminal — the "daemon unreachable" banner is the single most important
/// signal on the dashboard, so when the daemon is down the title is rendered as
/// loud reverse-video (a solid red bar with the terminal's background colour as
/// the text) which is unmissable on every theme. While healthy it stays a calm
/// bold cyan. Factoring this out of `render` lets a test assert it without a
/// terminal frame.
/// What: bold cyan when reachable; bold + reversed red when unreachable.
/// Test: `title_style_signals_daemon_health`.
fn title_style(daemon_reachable: bool) -> Style {
    let base = Style::default()
        .fg(title_color(daemon_reachable))
        .add_modifier(Modifier::BOLD);
    if daemon_reachable {
        base
    } else {
        base.add_modifier(Modifier::REVERSED)
    }
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

/// Build the styled controls-hint [`Line`] for the header area.
///
/// Why: the hint text rendered in the plain terminal default foreground was
/// invisible in iTerm2 (and many other terminals) because the header area
/// inherits a background that can be close to the default foreground color.
/// Bold + reversed video swaps the terminal's own fg/bg pair so the line is
/// guaranteed to be readable on every dark or light theme.
/// What: wraps `status_line(state)` with `Modifier::BOLD | Modifier::REVERSED`.
/// Test: `status_bar_line_is_high_contrast`.
pub fn status_bar_line(state: &DashboardState) -> Line<'static> {
    Line::from(status_line(state)).style(
        Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::REVERSED),
    )
}

/// Shared style for table header rows (Sessions, Circuit Breakers).
///
/// Why: header rows previously had no style, so column labels rendered in the
/// plain body color and were nearly indistinguishable from data rows. A bold,
/// reverse-video header reads clearly on both dark and light terminals because
/// it swaps whatever the terminal's own foreground/background pair is rather
/// than hard-coding a color that may clash with one theme.
/// What: bold + reversed (`Modifier::REVERSED`).
/// Test: `table_header_style_is_high_contrast`.
fn table_header_style() -> Style {
    Style::default()
        .add_modifier(Modifier::BOLD)
        .add_modifier(Modifier::REVERSED)
}

/// Build a styled panel title line for a bordered block.
///
/// Why: panel titles (`Sessions`, `Daemon Log`, ...) rendered in the plain
/// border color and were easy to miss. A bold cyan title is legible on both
/// dark and light terminals and matches the dashboard's header accent.
/// What: returns a bold cyan [`Line`] wrapping `text`.
/// Test: `panel_title_is_bold`.
fn panel_title(text: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(
        text.into(),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))
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
    let area = centered_rect(54, 13, frame.area());
    let text = help_text();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(text)
            .style(Style::default().fg(Color::White))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(panel_title("Help — press ? or Esc to close")),
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
        "  Enter     focus session + open output panel",
        "  p         pause selected session",
        "  r         resume selected session",
        "  x         stop selected session",
        "  o         open session in iTerm2 tab",
        "  : or /    activate the command bar",
        "  ?         toggle this help",
        "  Esc       deselect session / close help",
        "  q         quit",
    ]
    .join("\n")
}

/// Build the multi-line `/help` output shown in the command-bar output panel.
///
/// Why: the `/help` command must document every slash command inline; keeping
/// the text in one function lets a test assert each command is listed.
/// What: a header line plus one `  /<cmd>  — <description>` line per command.
/// Test: `command_help_lines_list_all_commands`.
pub fn command_help_lines() -> Vec<String> {
    vec![
        "Available commands:".to_string(),
        "  /pair             request a Telegram pairing code".to_string(),
        "  /projects         list discovered Claude Code projects".to_string(),
        "  /sessions         list daemon sessions".to_string(),
        "  /tmux             list tmux sessions (managed + external)".to_string(),
        "  /status           show daemon status".to_string(),
        "  /adopt <name>     adopt a tmux session by name".to_string(),
        "  /connect <id|dir>   focus session by id, or launch claude in dir".to_string(),
        "  /chat <message>   ask the LLM chat assistant".to_string(),
        "  /send <s> <cmd>   send a prompt to a Claude Code session".to_string(),
        "  /discover         auto-discover tmux sessions running Claude Code".to_string(),
        "  /help             show this help".to_string(),
        "  /exit, /quit      leave the dashboard".to_string(),
        "  <plain text>      send to the focused session + summarize".to_string(),
        "Tab: autocomplete   ↑/↓: history   Esc: close".to_string(),
    ]
}

/// Build the command-bar prompt prefix, reflecting the focused session.
///
/// Why: the summarized-chat mode routes plain text to a focused session; the
/// prompt must show which session is focused so the operator knows where plain
/// text will go.
/// What: `CMD [session: <name>]> ` when `active_session` is `Some`, otherwise
/// the bare `CMD> `.
/// Test: `command_prompt_reflects_focused_session`.
pub fn command_prompt(active_session: Option<&str>) -> String {
    match active_session {
        Some(name) => format!("CMD [session: {name}]> "),
        None => "CMD> ".to_string(),
    }
}

/// Build the input line shown in the command bar.
///
/// Why: kept separate so a test can assert the rendered prefix and the cursor
/// glyph without a terminal frame.
/// What: returns `<prompt><input>` plus a trailing `_` cursor when `active`,
/// where the prompt is [`command_prompt`] for `active_session`.
/// Test: `command_input_line_shows_cursor_when_active`,
/// `command_input_line_shows_focused_session`.
pub fn command_input_line(bar: &CommandBar, active_session: Option<&str>) -> String {
    let prompt = command_prompt(active_session);
    if bar.active {
        format!("{prompt}{}_", bar.input)
    } else {
        format!("{prompt}{}", bar.input)
    }
}

/// Render the persistent command zone (output panel + input line).
///
/// Why: the command bar is always visible at the bottom of the dashboard — an
/// output panel showing the last result above a single editable input line.
/// What: draws a bordered `List` of the bar's output lines into `output_area`
/// and a bordered `Paragraph` of [`command_input_line`] into `input_area`; the
/// input line is highlighted yellow while the bar is active.
/// Test: line content is covered by `command_input_line` and
/// `command_help_lines`; layout is exercised by the rendering smoke test.
fn render_command_zone(
    frame: &mut Frame,
    bar: &CommandBar,
    active_session: Option<&str>,
    output_area: Rect,
    input_area: Rect,
) {
    let output_items: Vec<ListItem> = if bar.output.is_empty() {
        vec![ListItem::new(
            "(no command output yet — press : or / to begin)",
        )]
    } else {
        bar.output
            .iter()
            .map(|l| ListItem::new(l.as_str()))
            .collect()
    };
    let output = List::new(output_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(panel_title("Command Output")),
    );
    frame.render_widget(output, output_area);

    let input_style = if bar.active {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        // Inactive: use the terminal's default foreground rather than a dim
        // `Gray` so the "[: or / to activate]" hint stays readable on all themes.
        Style::default()
    };
    let hint = if bar.active {
        " [Tab: autocomplete  ↑↓: history]"
    } else {
        " [: or / to activate]"
    };
    let input = Paragraph::new(format!("{}{hint}", command_input_line(bar, active_session)))
        .style(input_style)
        .block(Block::default().borders(Borders::ALL));
    frame.render_widget(input, input_area);
}

/// Draw the dashboard frame.
///
/// Why: the single entry point the event loop calls each tick.
/// What: a vertical layout spanning the whole terminal — a two-line header
/// (title + status bar); a flexing top area holding the session/breaker/event
/// panels; a full-width 3-line command-output strip; and a full-width 3-line
/// CMD> input strip at the very bottom. When `show_help` is set, a centred help
/// overlay floats over the layout.
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
    // Whole-terminal vertical split: header, a flexing top area for the
    // panels, then the full-width command-output strip and CMD> input strip
    // pinned to the very bottom.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // header (title + status bar)
            Constraint::Min(6),    // top panels (sessions/breakers + events/log)
            Constraint::Length(3), // full-width command output strip
            Constraint::Length(3), // full-width CMD> input strip
        ])
        .split(frame.area());

    // The top area is itself split vertically into the panel rows; this keeps
    // the panels inside the flexing chunk and never inside the command strips.
    let top = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),     // sessions + breakers
            Constraint::Length(10), // events + log tail
        ])
        .split(chunks[1]);

    // The title doubles as a daemon-health indicator: a calm cyan when the
    // daemon is reachable, a loud red when it is not, so the operator sees the
    // error state at a glance instead of reading the text.
    let title = if state.daemon_reachable {
        format!("trusty-mpm dashboard — {} session(s)", state.sessions.len())
    } else {
        "trusty-mpm dashboard — daemon unreachable".to_string()
    };
    let header = Paragraph::new(vec![
        Line::from(title).style(title_style(state.daemon_reachable)),
        // Status / controls bar: see `status_bar_line` for the contrast rationale.
        status_bar_line(state),
    ]);
    frame.render_widget(header, chunks[0]);

    // Middle row: sessions beside either the circuit-breaker panel (no focused
    // session) or the session-output detail panel (a session is focused).
    let middle = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(top[0]);

    let sessions = Table::new(
        session_rows(state, state.selected_session),
        [
            Constraint::Length(10),
            Constraint::Min(20),
            Constraint::Length(14),
            Constraint::Length(6),
        ],
    )
    .header(Row::new(vec!["ID", "WORKDIR", "STATUS", "DELEG"]).style(table_header_style()))
    .row_highlight_style(Style::default().add_modifier(Modifier::BOLD))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(panel_title("Sessions")),
    );
    frame.render_stateful_widget(sessions, middle[0], table_state);

    if let Some(focused) = state.active_session.as_deref() {
        // A session is focused: the right panel shows its live output/history.
        let items: Vec<ListItem> = session_output_panel_lines(state)
            .into_iter()
            .map(ListItem::new)
            .collect();
        let output = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(panel_title(format!("Session Output / History — {focused}"))),
        );
        frame.render_widget(output, middle[1]);
    } else {
        let breakers = Table::new(
            breaker_rows(state),
            [
                Constraint::Min(12),
                Constraint::Length(10),
                Constraint::Length(6),
            ],
        )
        .header(Row::new(vec!["AGENT", "STATE", "FAILS"]).style(table_header_style()))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(panel_title("Circuit Breakers")),
        );
        frame.render_widget(breakers, middle[1]);
    }

    // Events row: recent hook-event feed (50%) beside the daemon log tail (50%).
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(top[1]);

    let items: Vec<ListItem> = event_lines(state).into_iter().map(ListItem::new).collect();
    let events = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(panel_title("Recent Events")),
    );
    frame.render_widget(events, bottom[0]);

    let log_items: Vec<ListItem> = state
        .log_lines
        .iter()
        .map(|l| ListItem::new(l.as_str()))
        .collect();
    let log_panel = List::new(log_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(panel_title("Daemon Log")),
    );
    frame.render_widget(log_panel, bottom[1]);

    // The persistent command zone spans the full terminal width at the bottom:
    // a 3-line output strip above a 3-line CMD> input strip.
    render_command_zone(
        frame,
        &state.command_bar,
        state.active_session.as_deref(),
        chunks[2],
        chunks[3],
    );

    if state.show_help {
        render_help_overlay(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic test [`SessionId`] derived from a short label.
    ///
    /// Why: `SessionId` is a real UUID newtype, so tests need a stable UUID per
    /// label to assert against in `focus_on` / `submit_connect`.
    /// What: copies the label's bytes into a fixed 16-byte UUID buffer.
    /// Test: used by the dashboard test suite.
    fn sid(label: &str) -> trusty_mpm_core::session::SessionId {
        let mut bytes = [0u8; 16];
        for (slot, b) in bytes.iter_mut().zip(label.bytes()) {
            *slot = b;
        }
        trusty_mpm_core::session::SessionId(uuid::Uuid::from_bytes(bytes))
    }

    /// Build a `SessionRow` for tests.
    fn session(id: &str, workdir: &str, status: &str, name: &str) -> SessionRow {
        use trusty_mpm_core::session::SessionStatus;
        let status = match status {
            "active" => SessionStatus::Active,
            "paused" => SessionStatus::Paused,
            "stopped" => SessionStatus::Stopped,
            other => panic!("unhandled test status: {other}"),
        };
        SessionRow {
            id: sid(id),
            workdir: workdir.into(),
            status,
            active_delegations: 0,
            tmux_name: name.into(),
            last_seen: Default::default(),
        }
    }

    #[test]
    fn session_rows_empty_when_no_sessions() {
        let state = DashboardState::default();
        assert!(session_rows(&state, 0).is_empty());
    }

    #[test]
    fn session_rows_format_each_session() {
        let mut row = session("abcd1234", "/tmp/proj", "active", "tmpm-quiet-falcon");
        row.active_delegations = 2;
        let state = DashboardState {
            daemon_reachable: true,
            sessions: vec![row],
            ..DashboardState::default()
        };
        let rows = session_rows(&state, 0);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn selected_row_is_highlighted() {
        // `session_rows` with `selected = 0` builds two rows; the highlight
        // logic in `session_row_style` puts a high-contrast blue background
        // on the selected row only.
        let state = DashboardState {
            sessions: vec![
                session("a", "/p/a", "active", "tmpm-a"),
                session("b", "/p/b", "active", "tmpm-b"),
            ],
            ..DashboardState::default()
        };
        let rows = session_rows(&state, 0);
        assert_eq!(rows.len(), 2);
        // Row 0 is selected → solid blue bg + bold white fg for clear contrast.
        let selected = session_row_style(true);
        assert_eq!(selected.bg, Some(Color::Blue));
        assert_eq!(selected.fg, Some(Color::White));
        assert!(selected.add_modifier.contains(Modifier::BOLD));
        // Row 1 is not selected → no background highlight.
        assert_eq!(session_row_style(false).bg, None);
    }

    #[test]
    fn title_color_signals_daemon_health() {
        // The header title is a health indicator: cyan when the daemon is
        // reachable, a loud red when it is not so the operator cannot miss it.
        assert_eq!(title_color(true), Color::Cyan);
        assert_eq!(title_color(false), Color::Red);
    }

    #[test]
    fn title_style_signals_daemon_health() {
        // Healthy: calm bold cyan, no reverse video.
        let healthy = title_style(true);
        assert_eq!(healthy.fg, Some(Color::Cyan));
        assert!(healthy.add_modifier.contains(Modifier::BOLD));
        assert!(!healthy.add_modifier.contains(Modifier::REVERSED));
        // Unreachable: loud bold + reverse-video red banner — unmissable on any
        // terminal theme, light or dark.
        let down = title_style(false);
        assert_eq!(down.fg, Some(Color::Red));
        assert!(down.add_modifier.contains(Modifier::BOLD));
        assert!(down.add_modifier.contains(Modifier::REVERSED));
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
    fn event(event: trusty_mpm_core::hook::HookEvent, at: &str) -> EventRow {
        EventRow {
            session: sid("evt"),
            event,
            at: at.into(),
            payload: serde_json::Value::Null,
        }
    }

    /// Build an `EventRow` with an explicit payload.
    fn event_with_payload(
        event: trusty_mpm_core::hook::HookEvent,
        at: &str,
        payload: serde_json::Value,
    ) -> EventRow {
        EventRow {
            session: sid("evt"),
            event,
            at: at.into(),
            payload,
        }
    }

    #[test]
    fn event_detail_filechanged_shows_basename() {
        use trusty_mpm_core::hook::HookEvent;
        let payload = serde_json::json!({ "path": "/home/me/proj/src/foo.rs" });
        assert_eq!(event_detail(HookEvent::FileChanged, &payload), "foo.rs");
    }

    #[test]
    fn event_detail_tool_events_show_tool_name() {
        use trusty_mpm_core::hook::HookEvent;
        let payload = serde_json::json!({ "tool": "Bash" });
        assert_eq!(event_detail(HookEvent::PreToolUse, &payload), "Bash");
        assert_eq!(event_detail(HookEvent::PostToolUse, &payload), "Bash");
    }

    #[test]
    fn event_detail_session_events_show_name() {
        use trusty_mpm_core::hook::HookEvent;
        let payload = serde_json::json!({ "session_name": "my-project" });
        assert_eq!(
            event_detail(HookEvent::SessionStart, &payload),
            "my-project"
        );
    }

    #[test]
    fn event_detail_empty_when_no_useful_field() {
        use trusty_mpm_core::hook::HookEvent;
        assert_eq!(
            event_detail(HookEvent::FileChanged, &serde_json::Value::Null),
            ""
        );
        assert_eq!(
            event_detail(HookEvent::TokenUsageUpdate, &serde_json::json!({})),
            ""
        );
    }

    #[test]
    fn event_detail_fallback_uses_message_field() {
        use trusty_mpm_core::hook::HookEvent;
        let payload = serde_json::json!({ "message": "disk full" });
        assert_eq!(event_detail(HookEvent::Notification, &payload), "disk full");
    }

    #[test]
    fn event_lines_include_detail_column() {
        use trusty_mpm_core::hook::HookEvent;
        let state = DashboardState {
            events: vec![event_with_payload(
                HookEvent::FileChanged,
                "2024-01-01T00:00:00Z",
                serde_json::json!({ "path": "/a/b/foo.rs" }),
            )],
            ..DashboardState::default()
        };
        let lines = event_lines(&state);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("FileChanged"));
        assert!(lines[0].contains("foo.rs"));
    }

    #[test]
    fn session_output_panel_lines_uses_captured_output() {
        // A non-empty `session_output` is shown verbatim (trailing-limited).
        let state = DashboardState {
            active_session: Some("frontend".into()),
            session_output: vec!["line one".into(), "line two".into()],
            ..DashboardState::default()
        };
        let lines = session_output_panel_lines(&state);
        assert_eq!(lines, vec!["line one".to_string(), "line two".to_string()]);
    }

    #[test]
    fn session_output_panel_lines_caps_at_limit() {
        let state = DashboardState {
            active_session: Some("frontend".into()),
            session_output: vec!["x".to_string(); SESSION_OUTPUT_LIMIT + 10],
            ..DashboardState::default()
        };
        assert_eq!(
            session_output_panel_lines(&state).len(),
            SESSION_OUTPUT_LIMIT
        );
    }

    #[test]
    fn session_output_panel_lines_falls_back_to_events() {
        // With no captured tmux output, the panel shows the focused session's
        // recent hook events.
        use trusty_mpm_core::hook::HookEvent;
        let mut row = event_with_payload(
            HookEvent::PreToolUse,
            "2024-01-01T00:00:00Z",
            serde_json::json!({ "tool": "Bash" }),
        );
        row.session = sid("frontend");
        let state = DashboardState {
            active_session: Some("frontend".into()),
            sessions: vec![session("frontend", "/p", "active", "frontend")],
            events: vec![row],
            ..DashboardState::default()
        };
        let lines = session_output_panel_lines(&state);
        assert!(lines.iter().any(|l| l.contains("PreToolUse")));
        assert!(lines.iter().any(|l| l.contains("Bash")));
    }

    #[test]
    fn session_output_panel_lines_placeholder_when_no_focus() {
        let state = DashboardState::default();
        assert_eq!(
            session_output_panel_lines(&state),
            vec!["(no session focused)".to_string()]
        );
    }

    #[test]
    fn clear_active_session_drops_focus() {
        let mut state = DashboardState {
            active_session: Some("frontend".into()),
            session_output: vec!["stale".into()],
            ..DashboardState::default()
        };
        assert!(state.clear_active_session());
        assert!(state.active_session.is_none());
        assert!(state.session_output.is_empty());
        // Clearing again reports nothing was focused.
        assert!(!state.clear_active_session());
    }

    #[test]
    fn set_active_session_clears_stale_output() {
        let mut state = DashboardState {
            sessions: vec![session("aaa", "/p/a", "active", "frontend")],
            session_output: vec!["stale".into()],
            ..DashboardState::default()
        };
        assert_eq!(state.set_active_session(), Some("frontend".to_string()));
        assert!(state.session_output.is_empty());
    }

    #[test]
    fn event_lines_format_recent_events() {
        let state = DashboardState {
            events: vec![event(
                trusty_mpm_core::hook::HookEvent::PreToolUse,
                "2024-01-01T00:00:00Z",
            )],
            ..DashboardState::default()
        };
        let lines = event_lines(&state);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("PreToolUse"));
        assert!(lines[0].contains(&short_session(&sid("evt"))));
    }

    #[test]
    fn event_lines_cap_at_twenty() {
        let state = DashboardState {
            events: vec![
                event(
                    trusty_mpm_core::hook::HookEvent::Stop,
                    "2024-01-01T00:00:00Z"
                );
                50
            ],
            ..DashboardState::default()
        };
        assert_eq!(event_lines(&state).len(), 20);
    }

    #[test]
    fn short_session_extracts_prefix() {
        let id = trusty_mpm_core::session::SessionId(
            uuid::Uuid::parse_str("abcd1234-5678-90ab-cdef-1234567890ab").unwrap(),
        );
        assert_eq!(short_session(&id), "abcd1234");
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
        use trusty_mpm_core::hook::HookEvent;
        let state = DashboardState {
            events: vec![
                event(HookEvent::SessionStart, "2024-01-01T00:00:00Z"),
                event(HookEvent::PreToolUse, "2024-01-01T00:00:01Z"),
                event(HookEvent::SessionEnd, "2024-01-01T00:00:02Z"),
            ],
            ..DashboardState::default()
        };
        let lines = event_lines(&state);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("SessionStart"));
        assert!(lines[2].contains("SessionEnd"));
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
        // Unknown statuses fall back to white, not a dim gray, so they stay
        // readable on every terminal theme.
        assert_eq!(session_status_color("unknown"), Color::White);
        assert_eq!(session_status_color("anything-else"), Color::White);
    }

    #[test]
    fn breaker_state_colours() {
        assert_eq!(breaker_state_color("closed"), Color::Green);
        assert_eq!(breaker_state_color("half_open"), Color::Yellow);
        assert_eq!(breaker_state_color("open"), Color::Red);
        // Unknown breaker states fall back to readable white, not dim gray.
        assert_eq!(breaker_state_color("weird"), Color::White);
    }

    #[test]
    fn table_header_style_is_high_contrast() {
        // Header rows must stand out from body rows: bold + reverse video reads
        // clearly on both dark and light terminals without a hard-coded color.
        let style = table_header_style();
        assert!(style.add_modifier.contains(Modifier::BOLD));
        assert!(style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn panel_title_is_bold() {
        // Panel titles render bold cyan so they are clearly readable.
        let title = panel_title("Sessions");
        let span = &title.spans[0];
        assert_eq!(span.content, "Sessions");
        assert_eq!(span.style.fg, Some(Color::Cyan));
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn status_bar_line_is_high_contrast() {
        // The controls hint must be bold + reversed so it is legible on every
        // terminal theme (dark or light) without hard-coding a color.
        let state = DashboardState::default();
        let line = status_bar_line(&state);
        assert!(
            line.style.add_modifier.contains(Modifier::BOLD),
            "status bar line must be bold"
        );
        assert!(
            line.style.add_modifier.contains(Modifier::REVERSED),
            "status bar line must use reversed video for contrast"
        );
        // The text content must still carry the key hint.
        assert!(
            line.to_string().contains("navigate"),
            "status bar must include key hints"
        );
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
        assert!(state.focus_on(&sid("bbb").0.to_string()));
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
    fn resolve_connect_found() {
        // A unique name-prefix match focuses the row and reports "Connected to".
        let mut state = DashboardState {
            sessions: vec![
                session("aaa", "/p/a", "active", "frontend"),
                session("bbb", "/p/b", "active", "backend"),
            ],
            ..DashboardState::default()
        };
        let msg = state.resolve_connect("front");
        assert_eq!(state.selected_session, 0);
        assert_eq!(msg, format!("Connected to {}", sid("aaa").0));
    }

    #[test]
    fn resolve_connect_not_found() {
        // A target matching nothing reports "No session matched".
        let mut state = DashboardState {
            sessions: vec![session("aaa", "/p/a", "active", "frontend")],
            ..DashboardState::default()
        };
        assert_eq!(state.resolve_connect("zzz"), "No session matched");
    }

    #[test]
    fn resolve_connect_ambiguous() {
        // Two sessions sharing a name prefix yield an "Ambiguous:" status line.
        let mut state = DashboardState {
            sessions: vec![
                session("aaa", "/p/a", "active", "feature-a"),
                session("bbb", "/p/b", "active", "feature-b"),
            ],
            ..DashboardState::default()
        };
        assert!(state.resolve_connect("feature").starts_with("Ambiguous:"));
    }

    #[test]
    fn connect_action_focuses_session_with_matching_workdir() {
        // A directory target matching an existing session's workdir focuses
        // that session and reports "Connected to" — no launch.
        let mut state = DashboardState {
            sessions: vec![
                session("aaa", "/p/a", "active", "frontend"),
                session("bbb", "/p/b", "active", "backend"),
            ],
            ..DashboardState::default()
        };
        let action = state.connect_action("/p/b");
        assert_eq!(state.selected_session, 1);
        assert_eq!(
            action,
            ConnectAction::Resolved(format!("Connected to {}", sid("bbb").0))
        );
    }

    #[test]
    fn connect_action_matches_workdir_ignoring_trailing_slash() {
        // `/connect /p/a/` matches a session stored as `/p/a`.
        let mut state = DashboardState {
            sessions: vec![session("aaa", "/p/a", "active", "frontend")],
            ..DashboardState::default()
        };
        let action = state.connect_action("/p/a/");
        assert!(matches!(action, ConnectAction::Resolved(_)));
        assert_eq!(state.selected_session, 0);
    }

    #[test]
    fn connect_action_routes_unmatched_dir_to_launch() {
        // An absolute path with no matching session routes to the launch path,
        // carrying the directory through unchanged.
        let mut state = DashboardState {
            sessions: vec![session("aaa", "/p/a", "active", "frontend")],
            ..DashboardState::default()
        };
        assert_eq!(
            state.connect_action("/some/new/project"),
            ConnectAction::Launch("/some/new/project".to_string())
        );
    }

    #[test]
    fn connect_action_routes_tilde_dir_to_launch() {
        // A `~`-prefixed path is treated as a directory; with no session match
        // it routes to launch with `~` expanded against $HOME.
        // SAFETY: single-threaded test; no other thread reads HOME concurrently.
        unsafe { std::env::set_var("HOME", "/home/tester") };
        let mut state = DashboardState::default();
        assert_eq!(
            state.connect_action("~/work/proj"),
            ConnectAction::Launch("/home/tester/work/proj".to_string())
        );
    }

    #[test]
    fn connect_action_resolves_fuzzy_name() {
        // A non-directory target still uses the fuzzy id/name resolver.
        let mut state = DashboardState {
            sessions: vec![session("aaa", "/p/a", "active", "frontend")],
            ..DashboardState::default()
        };
        let action = state.connect_action("front");
        assert_eq!(
            action,
            ConnectAction::Resolved(format!("Connected to {}", sid("aaa").0))
        );
    }

    #[test]
    fn looks_like_dir_detects_paths() {
        assert!(looks_like_dir("/abs/path"));
        assert!(looks_like_dir("~/home/path"));
        assert!(looks_like_dir("  ~/spaced "));
        assert!(!looks_like_dir("frontend"));
        assert!(!looks_like_dir("tmpm-abc"));
    }

    #[test]
    fn expand_dir_expands_tilde() {
        // SAFETY: single-threaded test; no concurrent HOME readers.
        unsafe { std::env::set_var("HOME", "/home/tester") };
        assert_eq!(expand_dir("~/proj"), "/home/tester/proj");
        assert_eq!(expand_dir("~"), "/home/tester");
        assert_eq!(expand_dir("/abs/path"), "/abs/path");
    }

    #[test]
    fn normalize_workdir_strips_trailing_slash() {
        assert_eq!(normalize_workdir("/p/a/"), "/p/a");
        assert_eq!(normalize_workdir("/p/a"), "/p/a");
        assert_eq!(normalize_workdir("/"), "/");
    }

    #[test]
    fn help_text_lists_command_bar_binding() {
        assert!(help_text().contains("activate the command bar"));
    }

    #[test]
    fn normalize_command_strips_slash_and_case() {
        assert_eq!(normalize_command("/pair"), "pair");
        assert_eq!(normalize_command("pair"), "pair");
        assert_eq!(normalize_command("  /Pair "), "pair");
        assert_eq!(normalize_command(""), "");
    }

    #[test]
    fn known_commands_contains_core_verbs() {
        for verb in [
            "pair", "projects", "sessions", "tmux", "status", "adopt", "help",
        ] {
            assert!(
                KNOWN_COMMANDS.contains(&verb),
                "KNOWN_COMMANDS missing `{verb}`"
            );
        }
    }

    #[test]
    fn command_help_lines_list_all_commands() {
        let text = command_help_lines().join("\n");
        for verb in [
            "/pair",
            "/projects",
            "/sessions",
            "/tmux",
            "/status",
            "/adopt",
            "/connect",
        ] {
            assert!(text.contains(verb), "/help missing `{verb}`");
        }
    }

    #[test]
    fn command_bar_activate_deactivate() {
        // `:` / `/` activate the bar; Esc deactivates and clears the buffer.
        let mut bar = CommandBar::default();
        assert!(!bar.active);
        bar.activate();
        assert!(bar.active);
        bar.push('p');
        bar.deactivate();
        assert!(!bar.active);
        assert!(bar.input.is_empty());
    }

    #[test]
    fn command_bar_edits_buffer() {
        // Printable keys append, Backspace removes the trailing character;
        // both are no-ops while the bar is inactive.
        let mut bar = CommandBar::default();
        bar.push('x'); // inactive — ignored
        assert!(bar.input.is_empty());
        bar.activate();
        bar.push('p');
        bar.push('a');
        assert_eq!(bar.input, "pa");
        bar.backspace();
        assert_eq!(bar.input, "p");
    }

    #[test]
    fn command_bar_tab_cycles_matches() {
        // Tab on `/p` cycles `pair` then `projects`, wrapping back to `pair`.
        let mut bar = CommandBar::default();
        bar.activate();
        bar.push('p');
        bar.autocomplete();
        assert_eq!(bar.input, "pair");
        bar.autocomplete();
        assert_eq!(bar.input, "projects");
        bar.autocomplete();
        assert_eq!(bar.input, "pair");
    }

    #[test]
    fn command_bar_tab_no_match_is_noop() {
        // Tab with a prefix matching nothing leaves the buffer untouched.
        let mut bar = CommandBar::default();
        bar.activate();
        bar.push('z');
        bar.autocomplete();
        assert_eq!(bar.input, "z");
    }

    #[test]
    fn command_bar_submit_records_history() {
        // Enter takes the buffer and pushes a non-empty command into history.
        let mut bar = CommandBar::default();
        bar.activate();
        for c in "pair".chars() {
            bar.push(c);
        }
        let typed = bar.take_for_execution();
        assert_eq!(typed, "pair");
        assert!(bar.input.is_empty());
        assert_eq!(bar.history, vec!["pair".to_string()]);
        // An empty command is not recorded.
        let empty = bar.take_for_execution();
        assert_eq!(empty, "");
        assert_eq!(bar.history.len(), 1);
    }

    #[test]
    fn command_bar_history_capped_at_limit() {
        // History keeps only the last COMMAND_HISTORY_LIMIT commands.
        let mut bar = CommandBar::default();
        bar.activate();
        for n in 0..(COMMAND_HISTORY_LIMIT + 5) {
            for c in format!("cmd{n}").chars() {
                bar.push(c);
            }
            bar.take_for_execution();
        }
        assert_eq!(bar.history.len(), COMMAND_HISTORY_LIMIT);
        // The oldest entries were dropped; the newest is retained.
        assert_eq!(
            bar.history.last().map(String::as_str),
            Some(format!("cmd{}", COMMAND_HISTORY_LIMIT + 4).as_str())
        );
    }

    #[test]
    fn command_bar_history_recall() {
        // ↑ steps back through history; ↓ steps forward and clears past newest.
        let mut bar = CommandBar::default();
        bar.activate();
        for cmd in ["status", "tmux", "pair"] {
            for c in cmd.chars() {
                bar.push(c);
            }
            bar.take_for_execution();
        }
        bar.history_prev();
        assert_eq!(bar.input, "pair");
        bar.history_prev();
        assert_eq!(bar.input, "tmux");
        bar.history_prev();
        assert_eq!(bar.input, "status");
        bar.history_prev(); // saturates at the oldest
        assert_eq!(bar.input, "status");
        bar.history_next();
        assert_eq!(bar.input, "tmux");
        bar.history_next();
        assert_eq!(bar.input, "pair");
        bar.history_next(); // past newest → back to empty live input
        assert!(bar.input.is_empty());
    }

    #[test]
    fn command_bar_set_output() {
        // Output replaces the previous result.
        let mut bar = CommandBar::default();
        bar.set_output(vec!["first".into()]);
        assert_eq!(bar.output, vec!["first".to_string()]);
        bar.set_output(vec!["second".into(), "line".into()]);
        assert_eq!(bar.output, vec!["second".to_string(), "line".to_string()]);
    }

    #[test]
    fn command_input_line_shows_cursor_when_active() {
        let mut bar = CommandBar::default();
        for c in "pair".chars() {
            bar.input.push(c);
        }
        assert_eq!(command_input_line(&bar, None), "CMD> pair");
        bar.active = true;
        assert_eq!(command_input_line(&bar, None), "CMD> pair_");
    }

    #[test]
    fn command_prompt_reflects_focused_session() {
        // No focused session → bare prompt; a focused session is named inline.
        assert_eq!(command_prompt(None), "CMD> ");
        assert_eq!(
            command_prompt(Some("my-project")),
            "CMD [session: my-project]> "
        );
    }

    #[test]
    fn command_input_line_shows_focused_session() {
        // The input line carries the focused-session prefix.
        let bar = CommandBar::default();
        assert_eq!(
            command_input_line(&bar, Some("frontend")),
            "CMD [session: frontend]> "
        );
    }

    #[test]
    fn set_active_session_uses_selected_target() {
        // Focusing the highlighted row copies its tmux name into active_session.
        let mut state = DashboardState {
            sessions: vec![
                session("aaa", "/p/a", "active", "frontend"),
                session("bbb", "/p/b", "active", "backend"),
            ],
            selected_session: 1,
            ..DashboardState::default()
        };
        assert_eq!(state.set_active_session(), Some("backend".to_string()));
        assert_eq!(state.active_session.as_deref(), Some("backend"));

        // With no sessions, focusing yields None.
        let mut empty = DashboardState::default();
        assert_eq!(empty.set_active_session(), None);
        assert!(empty.active_session.is_none());
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
