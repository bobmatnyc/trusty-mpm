//! UI-agnostic command results.
//!
//! Why: the executor must hand a UI something structured — not a pre-formatted
//! string — so each UI (Telegram HTML, ratatui rows, CLI stdout) can render it
//! in its own idiom. [`CommandResult`] is that structured, transport-free
//! result type.
//! What: [`CommandResult`] enumerates one variant per command outcome, carrying
//! plain data the formatters consume. Errors are a variant, not a `Result::Err`,
//! so an unreachable daemon becomes a renderable message rather than a panic.
//! Test: `cargo test -p trusty-mpm-client` exercises the executor that produces
//! these; formatting is tested per UI crate.

/// A compact session summary for command results.
///
/// Why: a UI rendering `/sessions` needs the id, status, and workdir without
/// the full daemon `Session` shape.
/// What: the three fields every session list renders.
/// Test: covered by the executor's sessions test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// Session id (UUID string).
    pub id: String,
    /// Lifecycle status string.
    pub status: String,
    /// Working directory.
    pub workdir: String,
}

/// Recent overseer decision counts.
///
/// Why: the `/overseer` result reports how the overseer has been deciding.
/// What: the allow / block / flag tallies.
/// Test: covered by the executor's overseer test.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DecisionCounts {
    /// Number of `allow` decisions.
    pub allow: u64,
    /// Number of `block` decisions.
    pub block: u64,
    /// Number of `flag` decisions.
    pub flag: u64,
}

/// One tmux session summary for command results.
///
/// Why: the `/tmux` result lists tmux session names.
/// What: just the session name.
/// Test: covered by the executor's tmux test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxSessionSummary {
    /// tmux session name.
    pub name: String,
}

/// One Claude Code config recommendation summary.
///
/// Why: the `/config` result surfaces analyzer recommendations.
/// What: the recommendation id and message.
/// Test: covered by the executor's config test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecommendationSummary {
    /// Stable recommendation id.
    pub id: String,
    /// Human-readable description.
    pub message: String,
}

/// The structured, UI-agnostic outcome of executing a [`crate::TrustyCommand`].
///
/// Why: keeping the result structured (not a string) lets each UI format it in
/// its own idiom; an error variant keeps a dead daemon renderable.
/// What: one variant per command outcome, carrying plain data.
/// Test: produced by the executor, covered by `executor.rs` tests.
#[derive(Debug, Clone)]
pub enum CommandResult {
    /// `/sessions` — the managed session list.
    Sessions(Vec<SessionSummary>),
    /// `/status` — one session's status and recent event names.
    SessionDetail {
        /// Session id or name as supplied.
        id: String,
        /// Lifecycle status string (or `"unknown"` when not listed).
        status: String,
        /// Recent hook-event wire names, newest last.
        events: Vec<String>,
    },
    /// `/overseer` — the overseer's status and recent decision tally.
    OverseerStatus {
        /// Whether the overseer is enabled.
        enabled: bool,
        /// Active overseer strategy name.
        handler: String,
        /// Recent allow / block / flag counts.
        decisions: DecisionCounts,
    },
    /// `/tmux` — every tmux session on the daemon host.
    TmuxSessions(Vec<TmuxSessionSummary>),
    /// `/config` — Claude Code config analyzer recommendations.
    ConfigAnalysis {
        /// Project directory analyzed.
        project: String,
        /// The analyzer's recommendations (empty when the config is healthy).
        recommendations: Vec<RecommendationSummary>,
    },
    /// `/snapshot` — a captured tmux pane.
    Snapshot {
        /// tmux session name.
        session: String,
        /// Captured pane text (may be empty).
        output: String,
    },
    /// `/kill` — a session was killed.
    Killed {
        /// The session id or name that was killed.
        session_id: String,
    },
    /// `/approve` — a permission request was approved.
    Approved {
        /// The session id or name that was approved.
        session_id: String,
    },
    /// `/deny` — a permission request was denied.
    Denied {
        /// The session id or name that was denied.
        session_id: String,
    },
    /// `tm pair` — the daemon generated a one-time pairing code.
    PairCode {
        /// The pairing code.
        code: String,
        /// Seconds until the code expires.
        expires_in_seconds: u64,
    },
    /// `/pair <code>` — pairing completed successfully.
    PairSuccess {
        /// Human-readable description of the paired chat.
        chat_info: String,
    },
    /// `/start` or `/pair` with no code — the current pairing status.
    PairState {
        /// Whether a chat is currently paired with the daemon.
        paired: bool,
    },
    /// `/alerts` — the current alert subscription, one line per entry.
    AlertSubscriptions(Vec<String>),
    /// `/help` — the command list text.
    Help(String),
    /// Any failure rendered as a message rather than a panic.
    Error(String),
}
