//! The single command executor.
//!
//! Why: before this crate, three UIs each translated operator intent into
//! daemon HTTP calls independently. [`CommandExecutor`] is the *one* place that
//! mapping lives — every UI hands it a [`TrustyCommand`] and gets back a
//! [`CommandResult`]. A new endpoint is wired here once.
//! What: [`CommandExecutor`] owns a [`DaemonClient`] and exposes [`execute`]
//! (command → result) plus [`pair_confirm`] for the chat-id-carrying pairing
//! confirm that does not fit the pure `TrustyCommand → CommandResult` shape.
//! Unreachable-daemon errors become a [`CommandResult::Error`], never a panic.
//! Test: `cargo test -p trusty-mpm-client` covers the pure `/help` path and the
//! HTTP paths against an in-process test daemon.

use crate::client::DaemonClient;
use crate::command::{TrustyCommand, help_text};
use crate::result::{
    CommandResult, DecisionCounts, RecommendationSummary, SessionSummary, TmuxSessionSummary,
};

/// Translates [`TrustyCommand`]s into daemon HTTP calls.
///
/// Why: the single seam between UI intent and the daemon API; isolating it here
/// means the Telegram bot, the TUI, and the CLI never embed HTTP logic.
/// What: wraps a [`DaemonClient`]; [`execute`] runs one command end-to-end.
/// Test: `execute_help_returns_help`, `execute_sessions_against_test_daemon`.
pub struct CommandExecutor {
    /// The shared daemon HTTP client.
    client: DaemonClient,
}

impl CommandExecutor {
    /// Build an executor targeting `daemon_url`.
    ///
    /// Why: a UI constructs one executor for the daemon it was pointed at.
    /// What: wraps a fresh [`DaemonClient`] for `daemon_url`.
    /// Test: `execute_help_returns_help`.
    pub fn new(daemon_url: impl Into<String>) -> Self {
        Self {
            client: DaemonClient::new(daemon_url),
        }
    }

    /// The underlying daemon client.
    ///
    /// Why: a UI's alert loop and pairing flow need direct client access
    /// alongside command execution.
    /// What: returns a reference to the wrapped [`DaemonClient`].
    /// Test: covered by the pairing tests.
    pub fn client(&self) -> &DaemonClient {
        &self.client
    }

    /// Execute one [`TrustyCommand`] against the daemon.
    ///
    /// Why: the single dispatch point — every UI funnels intent through here.
    /// What: maps each command to daemon calls and returns a structured
    /// [`CommandResult`]; a transport failure becomes [`CommandResult::Error`].
    /// `Pair { code: Some(_) }` cannot complete here (it needs a chat id) and is
    /// reported as a state query — UIs must call [`Self::pair_confirm`] instead.
    /// Test: `execute_help_returns_help`, `execute_sessions_against_test_daemon`,
    /// `execute_kill_returns_killed`.
    pub async fn execute(&self, cmd: TrustyCommand) -> CommandResult {
        match cmd {
            TrustyCommand::Help => CommandResult::Help(help_text().to_string()),
            TrustyCommand::Alerts => CommandResult::AlertSubscriptions(vec![
                "Categories: Permission, Agent".to_string(),
                "Memory alerts: enabled".to_string(),
            ]),
            TrustyCommand::Sessions => self.sessions().await,
            TrustyCommand::Status { session_id } => self.status(&session_id).await,
            TrustyCommand::Approve { session_id } => self.decide(&session_id, true).await,
            TrustyCommand::Deny { session_id } => self.decide(&session_id, false).await,
            TrustyCommand::Overseer => self.overseer().await,
            TrustyCommand::Tmux => self.tmux().await,
            TrustyCommand::Config { project } => self.config(&project).await,
            TrustyCommand::Snapshot { session } => self.snapshot(&session).await,
            TrustyCommand::Kill { session_id } => self.kill(&session_id).await,
            TrustyCommand::Start => self.pair_state().await,
            TrustyCommand::Pair { code: None } => self.pair_state().await,
            TrustyCommand::Pair { code: Some(_) } => {
                // A code-carrying pair requires the caller's chat id, which is
                // not part of the command; UIs route those to `pair_confirm`.
                self.pair_state().await
            }
        }
    }

    /// Confirm a pairing code on behalf of a specific chat.
    ///
    /// Why: `POST /pair/confirm` needs the confirming chat's id, which is not
    /// carried by [`TrustyCommand::Pair`]; the bot adapter supplies it here.
    /// What: calls the daemon's confirm endpoint and maps the result to
    /// [`CommandResult::PairSuccess`] or [`CommandResult::Error`].
    /// Test: `pair_confirm_unknown_code_errors`.
    pub async fn pair_confirm(&self, code: &str, chat_id: i64) -> CommandResult {
        match self.client.pair_confirm(code, chat_id).await {
            Ok(confirm) if confirm.success => CommandResult::PairSuccess {
                chat_info: format!("chat {}", confirm.chat_id.unwrap_or(chat_id)),
            },
            Ok(confirm) => CommandResult::Error(
                confirm
                    .error
                    .unwrap_or_else(|| "invalid or expired code".to_string()),
            ),
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }

    /// Request a one-time pairing code from the daemon.
    ///
    /// Why: `tm pair` asks the local daemon for a code to display.
    /// What: calls `POST /pair/request` and maps it to [`CommandResult::PairCode`].
    /// Test: `pair_request_returns_code`.
    pub async fn pair_request(&self) -> CommandResult {
        match self.client.pair_request().await {
            Ok(req) => CommandResult::PairCode {
                code: req.code,
                expires_in_seconds: req.expires_in_seconds,
            },
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }

    /// `/sessions` — fetch and summarize the managed session list.
    async fn sessions(&self) -> CommandResult {
        match self.client.sessions().await {
            Ok(rows) => CommandResult::Sessions(
                rows.into_iter()
                    .map(|s| SessionSummary {
                        id: s.id.as_str().unwrap_or("?").to_string(),
                        status: s.status.as_str().unwrap_or("unknown").to_string(),
                        workdir: s.workdir,
                    })
                    .collect(),
            ),
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }

    /// `/status` — fetch one session's recent events.
    async fn status(&self, session_id: &str) -> CommandResult {
        match self.client.session_events(session_id).await {
            Ok(events) => {
                let names: Vec<String> = events
                    .iter()
                    .rev()
                    .take(5)
                    .rev()
                    .map(|e| e.event.clone())
                    .collect();
                CommandResult::SessionDetail {
                    id: session_id.to_string(),
                    status: "active".to_string(),
                    events: names,
                }
            }
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }

    /// `/approve` and `/deny` — verify the session, record a synthetic decision.
    ///
    /// Why: both share the same flow — confirm the session is known, then post a
    /// synthetic `PostToolUse` hook carrying `{"approved": bool}` so the
    /// decision is audited.
    /// What: lists sessions to confirm `session_id`, posts the hook, and returns
    /// the approve/deny result; an unknown session is an `Error`.
    /// Test: `execute_approve_unknown_session_errors`.
    async fn decide(&self, session_id: &str, approved: bool) -> CommandResult {
        let exists = match self.client.sessions().await {
            Ok(rows) => rows.iter().any(|s| s.id.as_str() == Some(session_id)),
            Err(e) => return CommandResult::Error(format!("daemon unreachable: {e}")),
        };
        if !exists {
            return CommandResult::Error(format!("session {session_id} not found"));
        }
        // Record the decision as a synthetic PostToolUse hook event.
        let hook_url = format!("{}/hooks", self.client.base_url());
        let _ = reqwest::Client::new()
            .post(&hook_url)
            .json(&serde_json::json!({
                "session_id": session_id,
                "event": "PostToolUse",
                "payload": { "approved": approved },
            }))
            .send()
            .await;
        if approved {
            CommandResult::Approved {
                session_id: session_id.to_string(),
            }
        } else {
            CommandResult::Denied {
                session_id: session_id.to_string(),
            }
        }
    }

    /// `/overseer` — fetch the overseer status.
    async fn overseer(&self) -> CommandResult {
        match self.client.overseer_status().await {
            Ok(snap) => CommandResult::OverseerStatus {
                enabled: snap.enabled,
                handler: snap.handler,
                decisions: DecisionCounts {
                    allow: snap.decisions.0,
                    block: snap.decisions.1,
                    flag: snap.decisions.2,
                },
            },
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }

    /// `/tmux` — list every tmux session on the host.
    async fn tmux(&self) -> CommandResult {
        match self.client.tmux_sessions().await {
            Ok(rows) => CommandResult::TmuxSessions(
                rows.into_iter()
                    .map(|r| TmuxSessionSummary { name: r.name })
                    .collect(),
            ),
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }

    /// `/config` — analyze a project's Claude Code config.
    async fn config(&self, project: &str) -> CommandResult {
        match self.client.analyze_config(project).await {
            Ok(recs) => CommandResult::ConfigAnalysis {
                project: project.to_string(),
                recommendations: recs
                    .into_iter()
                    .map(|r| RecommendationSummary {
                        id: r.id,
                        message: r.message,
                    })
                    .collect(),
            },
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }

    /// `/snapshot` — capture a tmux pane.
    async fn snapshot(&self, session: &str) -> CommandResult {
        match self.client.snapshot_tmux_session(session).await {
            Ok(Some(output)) => CommandResult::Snapshot {
                session: session.to_string(),
                output,
            },
            Ok(None) => CommandResult::Error(format!("tmux session {session} not found")),
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }

    /// `/kill` — kill a session.
    async fn kill(&self, session_id: &str) -> CommandResult {
        match self.client.kill_session(session_id).await {
            Ok(true) => CommandResult::Killed {
                session_id: session_id.to_string(),
            },
            Ok(false) => CommandResult::Error(format!("session {session_id} not found")),
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }

    /// `/start` and `/pair` (no code) — query the pairing status.
    async fn pair_state(&self) -> CommandResult {
        match self.client.pair_status().await {
            Ok(status) => CommandResult::PairState {
                paired: status.paired,
            },
            Err(e) => CommandResult::Error(format!("daemon unreachable: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::IntoFuture;

    /// Spawn the daemon's real HTTP API on a random loopback port.
    ///
    /// Why: lets the executor be tested against the genuine daemon routes
    /// without a live daemon, tmux, or external network.
    /// What: builds `api::router(DaemonState::shared())`, binds an ephemeral
    /// port, serves it on a background task, and returns the state plus base URL.
    /// Test: used by the `execute_*` tests below.
    async fn spawn_test_daemon() -> (
        std::sync::Arc<trusty_mpm_daemon::state::DaemonState>,
        String,
    ) {
        use trusty_mpm_daemon::{api, state::DaemonState};
        let state = DaemonState::shared();
        let router = api::router(std::sync::Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router).into_future());
        (state, format!("http://{addr}"))
    }

    #[tokio::test]
    async fn execute_help_returns_help() {
        // The `/help` path is pure — no HTTP, no daemon.
        let executor = CommandExecutor::new("http://unused");
        match executor.execute(TrustyCommand::Help).await {
            CommandResult::Help(text) => assert!(text.contains("/sessions")),
            other => panic!("expected Help, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_sessions_against_test_daemon() {
        // With one registered session, `/sessions` returns exactly that summary.
        use trusty_mpm_core::session::{ControlModel, Session, SessionId, SessionStatus};
        let (state, url) = spawn_test_daemon().await;
        let mut session = Session::new(SessionId::new(), "/tmp/proj", ControlModel::Tmux);
        session.status = SessionStatus::Active;
        state.register_session(session);

        let executor = CommandExecutor::new(url);
        match executor.execute(TrustyCommand::Sessions).await {
            CommandResult::Sessions(list) => {
                assert_eq!(list.len(), 1);
                assert_eq!(list[0].workdir, "/tmp/proj");
            }
            other => panic!("expected Sessions, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_kill_returns_killed() {
        // Registering a session then killing it yields `Killed`.
        use trusty_mpm_core::session::{ControlModel, Session, SessionId, SessionStatus};
        let (state, url) = spawn_test_daemon().await;
        let id = SessionId::new();
        let mut session = Session::new(id, "/tmp/proj", ControlModel::Tmux);
        session.status = SessionStatus::Active;
        state.register_session(session);

        let executor = CommandExecutor::new(url);
        match executor
            .execute(TrustyCommand::Kill {
                session_id: id.0.to_string(),
            })
            .await
        {
            CommandResult::Killed { session_id } => assert_eq!(session_id, id.0.to_string()),
            other => panic!("expected Killed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_kill_unknown_session_errors() {
        let (_state, url) = spawn_test_daemon().await;
        let executor = CommandExecutor::new(url);
        match executor
            .execute(TrustyCommand::Kill {
                session_id: uuid::Uuid::new_v4().to_string(),
            })
            .await
        {
            CommandResult::Error(msg) => assert!(msg.contains("not found")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_approve_unknown_session_errors() {
        let (_state, url) = spawn_test_daemon().await;
        let executor = CommandExecutor::new(url);
        match executor
            .execute(TrustyCommand::Approve {
                session_id: uuid::Uuid::new_v4().to_string(),
            })
            .await
        {
            CommandResult::Error(msg) => assert!(msg.contains("not found")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_approve_known_session() {
        use trusty_mpm_core::session::{ControlModel, Session, SessionId, SessionStatus};
        let (state, url) = spawn_test_daemon().await;
        let id = SessionId::new();
        let mut session = Session::new(id, "/tmp/proj", ControlModel::Tmux);
        session.status = SessionStatus::Active;
        state.register_session(session);

        let executor = CommandExecutor::new(url);
        match executor
            .execute(TrustyCommand::Approve {
                session_id: id.0.to_string(),
            })
            .await
        {
            CommandResult::Approved { session_id } => assert_eq!(session_id, id.0.to_string()),
            other => panic!("expected Approved, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_overseer_returns_status() {
        let (_state, url) = spawn_test_daemon().await;
        let executor = CommandExecutor::new(url);
        match executor.execute(TrustyCommand::Overseer).await {
            CommandResult::OverseerStatus { handler, .. } => assert!(!handler.is_empty()),
            other => panic!("expected OverseerStatus, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_status_no_events() {
        use trusty_mpm_core::session::{ControlModel, Session, SessionId, SessionStatus};
        let (state, url) = spawn_test_daemon().await;
        let id = SessionId::new();
        let mut session = Session::new(id, "/tmp/proj", ControlModel::Tmux);
        session.status = SessionStatus::Active;
        state.register_session(session);

        let executor = CommandExecutor::new(url);
        match executor
            .execute(TrustyCommand::Status {
                session_id: id.0.to_string(),
            })
            .await
        {
            CommandResult::SessionDetail { events, .. } => assert!(events.is_empty()),
            other => panic!("expected SessionDetail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pair_request_returns_code() {
        let (_state, url) = spawn_test_daemon().await;
        let executor = CommandExecutor::new(url);
        match executor.pair_request().await {
            CommandResult::PairCode { code, .. } => assert_eq!(code.len(), 6),
            other => panic!("expected PairCode, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pair_confirm_unknown_code_errors() {
        let (_state, url) = spawn_test_daemon().await;
        let executor = CommandExecutor::new(url);
        match executor.pair_confirm("ZZZZZZ", 999).await {
            CommandResult::Error(msg) => assert!(msg.contains("invalid")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn pair_request_then_confirm_succeeds() {
        // The full handshake: request a code, confirm it, then status is paired.
        let (_state, url) = spawn_test_daemon().await;
        let executor = CommandExecutor::new(url);
        let code = match executor.pair_request().await {
            CommandResult::PairCode { code, .. } => code,
            other => panic!("expected PairCode, got {other:?}"),
        };
        match executor.pair_confirm(&code, 424242).await {
            CommandResult::PairSuccess { chat_info } => assert!(chat_info.contains("424242")),
            other => panic!("expected PairSuccess, got {other:?}"),
        }
        match executor.execute(TrustyCommand::Start).await {
            CommandResult::PairState { paired } => assert!(paired),
            other => panic!("expected PairState, got {other:?}"),
        }
    }
}
