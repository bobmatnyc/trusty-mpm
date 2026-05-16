//! trusty-mpm Telegram bot library.
//!
//! Why: remote management lets an operator drive the daemon from a phone —
//! list sessions, check status, approve a pending permission request, and
//! receive memory-pressure / hook-event alerts — without a terminal. Exposing
//! the bot as a library lets the unified `trusty-mpm telegram` subcommand reuse
//! it without a separate binary.
//! What: parses operator commands via [`commands`] and decides/format alerts
//! via [`alerts`]. The teloxide runtime wiring in [`run`] is intentionally
//! thin; all decision logic lives in the two unit-tested modules.
//! Test: `cargo test -p trusty-mpm-telegram` covers command parsing and alert
//! formatting; `trusty-mpm telegram --check` validates config.

pub mod alerts;
pub mod commands;

use teloxide::prelude::*;

use alerts::AlertConfig;

/// Run the Telegram remote-management bot against `url`.
///
/// Why: shared entry point for both the `trusty-mpm telegram` subcommand and
/// the backward-compatible `trusty-mpm-telegram` shim binary.
/// What: with `check`, prints the resolved configuration and exits; otherwise
/// boots the teloxide long-polling repl, dispatching each text message through
/// [`handle_command`] against the daemon HTTP API.
/// Test: `--check` mode is deterministic; live behaviour is exercised by
/// running the bot against a daemon. Command handling is covered by tests.
pub async fn run(url: String, token: Option<String>, check: bool) -> anyhow::Result<()> {
    let alert_config = AlertConfig::recommended();

    if check {
        println!("trusty-mpm Telegram bot configuration:");
        println!("  daemon url        : {url}");
        println!(
            "  token configured  : {}",
            if token.is_some() { "yes" } else { "no" }
        );
        println!("  alert categories  : {:?}", alert_config.categories);
        println!("  memory alerts     : {}", alert_config.memory_alerts);
        println!();
        println!("{}", commands::help_text());
        return Ok(());
    }

    let token = token.ok_or_else(|| {
        anyhow::anyhow!("TELEGRAM_BOT_TOKEN is required (or pass --check to validate config)")
    })?;

    // Run the teloxide long-polling repl: every text message is parsed into a
    // `BotCommand` and dispatched against the daemon's HTTP API.
    let bot = Bot::new(token);
    let daemon_url = url.clone();

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let daemon_url = daemon_url.clone();
        async move {
            if let Some(text) = msg.text() {
                let reply = match commands::parse(text) {
                    Ok(cmd) => handle_command(cmd, &daemon_url).await,
                    Err(e) => e,
                };
                bot.send_message(msg.chat.id, reply).await?;
            }
            Ok(())
        }
    })
    .await;
    Ok(())
}

/// Dispatch a parsed operator command against the daemon HTTP API.
///
/// Why: keeps the teloxide closure thin — all daemon I/O and reply formatting
/// lives here where it can evolve independently of the runtime wiring.
/// What: maps each [`commands::BotCommand`] to a daemon request and renders a
/// human-readable reply string; unreachable-daemon and parse errors become the
/// reply text rather than panics.
/// Test: covered indirectly by `commands` parsing tests; live behaviour is
/// exercised by running the bot against a daemon.
pub async fn handle_command(cmd: commands::BotCommand, daemon_url: &str) -> String {
    use commands::BotCommand::*;
    let client = reqwest::Client::new();
    match cmd {
        Help => commands::help_text().to_string(),
        Sessions => {
            let url = format!("{daemon_url}/sessions");
            match client.get(&url).send().await {
                Ok(r) => match r.json::<serde_json::Value>().await {
                    Ok(body) => {
                        let sessions = body["sessions"].as_array().cloned().unwrap_or_default();
                        if sessions.is_empty() {
                            "No active sessions.".into()
                        } else {
                            sessions
                                .iter()
                                .map(|s| {
                                    format!(
                                        "{} {} {}",
                                        s["id"].as_str().unwrap_or("?"),
                                        s["status"].as_str().unwrap_or("?"),
                                        s["workdir"].as_str().unwrap_or("?")
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                    }
                    Err(e) => format!("parse error: {e}"),
                },
                Err(e) => format!("daemon unreachable: {e}"),
            }
        }
        Status { session_id } => {
            let url = format!("{daemon_url}/sessions/{session_id}/events");
            match client.get(&url).send().await {
                Ok(r) => match r.json::<serde_json::Value>().await {
                    Ok(body) => {
                        let events = body["events"].as_array().cloned().unwrap_or_default();
                        let last5: Vec<_> = events.iter().rev().take(5).collect();
                        if last5.is_empty() {
                            format!("Session {session_id}: no recent events")
                        } else {
                            last5
                                .iter()
                                .map(|e| e["event"].as_str().unwrap_or("?").to_string())
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                    }
                    Err(e) => format!("parse error: {e}"),
                },
                Err(e) => format!("daemon unreachable: {e}"),
            }
        }
        Approve { session_id } | Deny { session_id } => {
            format!("Permission approval for {session_id} not yet wired to daemon API")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commands::BotCommand;

    /// Spawn the daemon's real HTTP API on a random loopback port.
    ///
    /// Why: lets the Telegram command handler be tested against the genuine
    /// daemon routes without a live daemon, tmux, or external network.
    /// What: builds `api::router(DaemonState::shared())`, binds an ephemeral
    /// port, serves it on a background task, and returns the state plus base URL.
    /// Test: used by the `handle_*` tests below.
    async fn spawn_test_daemon() -> (
        std::sync::Arc<trusty_mpm_daemon::state::DaemonState>,
        String,
    ) {
        use std::future::IntoFuture;
        use trusty_mpm_daemon::{api, state::DaemonState};
        let state = DaemonState::shared();
        let router = api::router(std::sync::Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router).into_future());
        (state, format!("http://{addr}"))
    }

    #[tokio::test]
    async fn handle_help_returns_help_text() {
        // Pure branch: `Help` echoes the static help text, no HTTP needed.
        let reply = handle_command(BotCommand::Help, "http://unused").await;
        assert_eq!(reply, commands::help_text());
    }

    #[tokio::test]
    async fn handle_approve_contains_session_id() {
        let reply = handle_command(
            BotCommand::Approve {
                session_id: "sess-42".into(),
            },
            "http://unused",
        )
        .await;
        assert!(reply.contains("sess-42"));
    }

    #[tokio::test]
    async fn handle_deny_contains_session_id() {
        let reply = handle_command(
            BotCommand::Deny {
                session_id: "sess-99".into(),
            },
            "http://unused",
        )
        .await;
        assert!(reply.contains("sess-99"));
    }

    #[tokio::test]
    async fn handle_sessions_with_no_sessions_returns_empty_msg() {
        let (_state, url) = spawn_test_daemon().await;
        let reply = handle_command(BotCommand::Sessions, &url).await;
        assert_eq!(reply, "No active sessions.");
    }

    #[tokio::test]
    async fn handle_sessions_lists_one_session() {
        use trusty_mpm_core::session::{ControlModel, Session, SessionId, SessionStatus};
        let (state, url) = spawn_test_daemon().await;
        let mut session = Session::new(SessionId::new(), "/tmp/proj", ControlModel::Tmux);
        session.status = SessionStatus::Active;
        state.register_session(session);
        let reply = handle_command(BotCommand::Sessions, &url).await;
        assert!(reply.contains("/tmp/proj"));
        assert_ne!(reply, "No active sessions.");
    }

    #[tokio::test]
    async fn handle_sessions_daemon_unreachable_returns_error() {
        // Port 1 is never bound by a daemon; the handler must report it.
        let reply = handle_command(BotCommand::Sessions, "http://127.0.0.1:1").await;
        assert!(reply.contains("unreachable"));
    }

    #[tokio::test]
    async fn handle_status_no_events_returns_message() {
        use trusty_mpm_core::session::{ControlModel, Session, SessionId, SessionStatus};
        let (state, url) = spawn_test_daemon().await;
        let id = SessionId::new();
        let mut session = Session::new(id, "/tmp/proj", ControlModel::Tmux);
        session.status = SessionStatus::Active;
        state.register_session(session);
        let reply = handle_command(
            BotCommand::Status {
                session_id: id.0.to_string(),
            },
            &url,
        )
        .await;
        assert!(reply.contains("no recent events"));
    }
}
