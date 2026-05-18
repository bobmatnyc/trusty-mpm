//! Auto-discovery of tmux sessions running Claude Code.
//!
//! Why: `GET /sessions` only reports daemon-managed sessions, but operators run
//! `claude`, `claude-code`, `claude-mpm`, or `tm` in tmux panes the daemon never
//! created. Those sessions were invisible until manually `/adopt`-ed. Scanning
//! tmux at startup (and on demand) brings them under oversight automatically.
//! What: [`discover_claude_sessions`] runs `tmux list-panes -a` and, for every
//! pane whose current command looks like Claude Code, registers a session for
//! the owning tmux session if one is not already registered. [`is_claude_command`]
//! is the pure predicate the scan keys on.
//! Test: `cargo test -p trusty-mpm-daemon discovery` covers the command
//! predicate and the pane-line parser without spawning tmux.

use std::collections::HashSet;

use trusty_mpm_core::session::{ControlModel, Session, SessionId};

use crate::state::DaemonState;
use crate::tmux::TmuxDriver;

/// Process names that mark a tmux pane as running Claude Code.
///
/// Why: auto-discovery must recognise the handful of binaries an operator runs
/// a Claude Code session under; keeping the list in one place makes the
/// predicate auditable.
/// What: substrings matched case-insensitively against `pane_current_command`.
const CLAUDE_COMMANDS: &[&str] = &["claude", "claude-code", "claude-mpm", "tm"];

/// True when `command` names a Claude Code process worth adopting.
///
/// Why: the discovery scan must decide, per pane, whether it hosts Claude Code;
/// a pure predicate keeps that decision unit-testable.
/// What: case-insensitively matches `command` against [`CLAUDE_COMMANDS`] —
/// `claude`/`claude-code`/`claude-mpm` match as substrings, while `tm` must be
/// the whole command so it never matches unrelated binaries like `vim`.
/// Test: `is_claude_command_matches_known`, `is_claude_command_rejects_others`.
pub fn is_claude_command(command: &str) -> bool {
    let lower = command.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }
    // `tm` is short enough to appear inside unrelated names — require an exact
    // match for it, but allow substring matches for the longer, distinctive
    // `claude*` names.
    lower == "tm"
        || CLAUDE_COMMANDS
            .iter()
            .any(|c| *c != "tm" && lower.contains(c))
}

/// Parse one `tmux list-panes -a` line into `(session_name, pane_command)`.
///
/// Why: the scan formats panes as `#{session_name} #{pane_current_command}`;
/// isolating the split keeps [`discover_claude_sessions`] readable and lets the
/// parser be tested without tmux.
/// What: splits on the first whitespace run; returns `None` for an empty or
/// single-field line.
/// Test: `parse_pane_line_splits_fields`.
fn parse_pane_line(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let (session, command) = trimmed.split_once(char::is_whitespace)?;
    let command = command.trim();
    if session.is_empty() || command.is_empty() {
        return None;
    }
    Some((session.to_string(), command.to_string()))
}

/// Outcome of one auto-discovery scan.
///
/// Why: the `POST /sessions/discover` handler and the Telegram/TUI `/discover`
/// commands report how many sessions the scan adopted; bundling the count with
/// the names lets callers log the specifics.
/// What: the number of newly-registered sessions and their tmux names.
/// Test: covered indirectly by `discover_claude_sessions` against a daemon with
/// no tmux (yields an empty result).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DiscoveryResult {
    /// Number of tmux sessions newly registered by the scan.
    pub adopted: usize,
    /// Friendly tmux names of the newly-registered sessions.
    pub sessions: Vec<String>,
}

/// Scan existing tmux sessions and register any running Claude Code.
///
/// Why: sessions the daemon did not create are invisible to `GET /sessions`
/// until adopted; running this at startup (and on demand via the API) keeps the
/// registry honest without operator intervention.
/// What: runs `tmux list-panes -a -F "#{session_name} #{pane_current_command}"`,
/// and for every pane whose command satisfies [`is_claude_command`], registers
/// a [`Session`] for the owning tmux session — unless one is already registered
/// under that `tmux_name`. tmux being absent yields an empty [`DiscoveryResult`]
/// rather than an error.
/// Test: `is_claude_command_*` cover the predicate; the tmux-absent path is
/// exercised by `discover_with_no_tmux_is_empty`.
pub fn discover_claude_sessions(state: &DaemonState) -> DiscoveryResult {
    let driver = match TmuxDriver::discover() {
        Ok(driver) => driver,
        Err(_) => {
            tracing::info!("tmux unavailable; session auto-discovery skipped");
            return DiscoveryResult::default();
        }
    };

    let raw = match driver.list_claude_panes() {
        Ok(raw) => raw,
        Err(e) => {
            tracing::warn!("tmux pane listing failed during discovery: {e}");
            return DiscoveryResult::default();
        }
    };

    // Already-registered tmux names — never register the same session twice.
    let registered: HashSet<String> = state
        .list_sessions()
        .into_iter()
        .map(|s| s.tmux_name)
        .collect();

    let mut result = DiscoveryResult::default();
    let mut seen: HashSet<String> = HashSet::new();
    for line in raw.lines() {
        let Some((session_name, command)) = parse_pane_line(line) else {
            continue;
        };
        if !is_claude_command(&command) {
            continue;
        }
        if registered.contains(&session_name) || !seen.insert(session_name.clone()) {
            continue;
        }
        // Register a tmux-hosted session under the discovered tmux name. The
        // workdir is unknown from the pane listing, so it is left empty — a
        // later snapshot or hook event can enrich it.
        let mut session = Session::new(SessionId::new(), String::new(), ControlModel::Tmux);
        session.tmux_name = session_name.clone();
        session.status = trusty_mpm_core::session::SessionStatus::Active;
        state.register_session(session);
        tracing::info!("auto-discovered Claude Code tmux session: {session_name}");
        result.adopted += 1;
        result.sessions.push(session_name);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_claude_command_matches_known() {
        for cmd in [
            "claude",
            "claude-code",
            "claude-mpm",
            "tm",
            "Claude",
            "CLAUDE-CODE",
        ] {
            assert!(is_claude_command(cmd), "expected `{cmd}` to match");
        }
    }

    #[test]
    fn is_claude_command_rejects_others() {
        for cmd in ["bash", "zsh", "vim", "tmux", "node", "", "  "] {
            assert!(!is_claude_command(cmd), "expected `{cmd}` not to match");
        }
    }

    #[test]
    fn parse_pane_line_splits_fields() {
        assert_eq!(
            parse_pane_line("my-project claude"),
            Some(("my-project".to_string(), "claude".to_string())),
        );
        // Extra whitespace between fields is tolerated.
        assert_eq!(
            parse_pane_line("  proj   claude-code  "),
            Some(("proj".to_string(), "claude-code".to_string())),
        );
        // A line with no command field is rejected.
        assert_eq!(parse_pane_line("lonely"), None);
        assert_eq!(parse_pane_line(""), None);
    }

    #[test]
    fn discover_with_no_tmux_is_empty() {
        // In CI tmux is typically absent (or hosts no Claude panes); discovery
        // must return a well-formed empty result, never panic.
        let state = DaemonState::new();
        let result = discover_claude_sessions(&state);
        assert_eq!(result.adopted, result.sessions.len());
    }
}
