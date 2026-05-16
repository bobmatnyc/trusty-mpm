//! Daemon HTTP API.
//!
//! Why: the CLI, TUI, and Telegram bot are separate processes; they need a
//! transport to the daemon. HTTP/JSON over a loopback port is simple, debuggable
//! with `curl`, and lets the universal hook relay receive events from a tiny
//! forwarder shim with no client library.
//! What: builds the axum [`Router`] — health, session listing, the hook-event
//! relay endpoint, the live event feed, and the per-agent breaker view. State
//! is injected as `Arc<DaemonState>` via axum's `State` extractor.
//! Test: `cargo test -p trusty-mpm-daemon` drives the handlers directly with an
//! in-memory state (no socket bind needed).

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
};
use trusty_mpm_core::hook::{HookEvent, HookEventRecord};
use trusty_mpm_core::session::{ControlModel, Session, SessionId, SessionStatus};
use trusty_mpm_core::tmux::TmuxTarget;

use crate::state::DaemonState;
use crate::tmux::TmuxDriver;

/// Build the daemon's HTTP router with shared state injected.
///
/// Why: one place wires every route so `main` stays a thin bootstrap.
/// What: returns an axum `Router` already carrying `Arc<DaemonState>`.
/// Test: `health_endpoint_responds` and the hook-relay tests call handlers via
/// this router's logic.
pub fn router(state: Arc<DaemonState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/sessions", get(list_sessions).post(register_session))
        .route("/sessions/{id}", axum::routing::delete(remove_session))
        .route("/sessions/{id}/events", get(session_events))
        .route("/events", get(recent_events))
        .route("/hooks", post(ingest_hook))
        .route("/breakers", get(breakers))
        .with_state(state)
}

/// Liveness probe — always returns `ok` while the daemon is up.
async fn health() -> &'static str {
    "ok"
}

/// `GET /sessions` — snapshot of every managed session.
async fn list_sessions(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "sessions": state.list_sessions() }))
}

/// `GET /events` — recent hook events across all sessions (dashboard feed).
async fn recent_events(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "events": state.recent_hook_events() }))
}

/// JSON body for registering a session via `POST /sessions`.
///
/// Why: a session created by an external launcher (or the CLI) must announce
/// itself so the dashboard and MCP tools can see it.
/// What: the working directory the session runs in.
/// Test: `register_and_remove_session`.
#[derive(serde::Deserialize)]
pub struct RegisterSession {
    /// Working directory the session was launched in.
    pub workdir: String,
}

/// `POST /sessions` — register a new managed session, returning its id.
///
/// Why: registering a session should also stand up its tmux host so the
/// operator gets a live Claude Code session, not just a bookkeeping entry.
/// What: builds the `Session`, then best-effort launches a detached tmux
/// session and starts `claude` in it; tmux failures are logged, not fatal —
/// the session is still registered so the API stays usable without tmux.
/// Test: `register_and_remove_session` covers the bookkeeping path.
async fn register_session(
    State(state): State<Arc<DaemonState>>,
    Json(body): Json<RegisterSession>,
) -> Json<serde_json::Value> {
    let session = Session {
        id: SessionId::new(),
        workdir: body.workdir.clone(),
        status: SessionStatus::Starting,
        control: ControlModel::Tmux,
        active_delegations: 0,
    };
    let id = session.id;
    state.register_session(session);

    // Best-effort: launch the session inside tmux. Any failure is logged and
    // the session stays registered in the `Starting` state.
    let tmux_name = format!("trusty-mpm-{}", id.0);
    match TmuxDriver::discover() {
        Ok(driver) => {
            if let Err(e) = driver.create_session(&tmux_name, Some(&body.workdir)) {
                tracing::warn!("tmux create_session failed: {e}");
            } else {
                let target = TmuxTarget::session(&tmux_name);
                if let Err(e) = driver.send_line(&target, "claude") {
                    tracing::warn!("tmux send_line failed: {e}");
                } else if let Some(mut sess) = state.session(id) {
                    sess.status = SessionStatus::Active;
                    state.register_session(sess);
                }
            }
        }
        Err(_) => {
            tracing::info!("tmux unavailable; session {id:?} registered without tmux launch");
        }
    }

    Json(serde_json::json!({ "id": id }))
}

/// `DELETE /sessions/:id` — deregister a session.
async fn remove_session(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = parse_id(&id)?;
    match state.remove_session(session) {
        Some(_) => Ok(Json(serde_json::json!({ "removed": id }))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// `GET /sessions/:id/events` — recent hook events for one session.
async fn session_events(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = parse_id(&id)?;
    Ok(Json(serde_json::json!({
        "events": state.hook_events_for(session),
    })))
}

/// `GET /breakers` — every agent's circuit-breaker state.
async fn breakers(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let breakers: Vec<_> = state
        .all_breakers()
        .into_iter()
        .map(|(agent, cb)| serde_json::json!({ "agent": agent, "breaker": cb }))
        .collect();
    Json(serde_json::json!({ "breakers": breakers }))
}

/// JSON body for the universal hook relay endpoint.
///
/// Why: the forwarder shim posts raw Claude Code hook events here; a typed
/// body documents the contract.
/// What: session id, the Claude Code event name, and the opaque payload.
/// Test: `hook_relay_ingests_known_event`.
#[derive(serde::Deserialize)]
pub struct HookPost {
    /// Session the event came from (UUID string).
    pub session_id: String,
    /// Claude Code event name, e.g. `PreToolUse`.
    pub event: String,
    /// Raw event payload (shape varies per event).
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// `POST /hooks` — universal hook relay; ingests one Claude Code hook event.
///
/// Why: this is how the daemon achieves full observability — a forwarder shim
/// configured for *all* 32 hook events posts each one here.
/// What: parses the session id and event name, appends a `HookEventRecord` to
/// the ring buffer; rejects unknown events/ids with `400`.
/// Test: `hook_relay_ingests_known_event`, `hook_relay_rejects_unknown_event`.
async fn ingest_hook(
    State(state): State<Arc<DaemonState>>,
    Json(post): Json<HookPost>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = parse_id(&post.session_id)?;
    let event = HookEvent::from_wire(&post.event).ok_or(StatusCode::BAD_REQUEST)?;
    state.push_hook_event(HookEventRecord::now(session, event, post.payload));
    Ok(Json(serde_json::json!({ "accepted": post.event })))
}

/// Parse a UUID string into a `SessionId`, mapping failure to `400`.
fn parse_id(raw: &str) -> Result<SessionId, StatusCode> {
    uuid::Uuid::parse_str(raw)
        .map(SessionId)
        .map_err(|_| StatusCode::BAD_REQUEST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trusty_mpm_core::session::{ControlModel, Session, SessionStatus};

    fn state_with_session() -> (Arc<DaemonState>, SessionId) {
        let state = DaemonState::shared();
        let id = SessionId::new();
        state.register_session(Session {
            id,
            workdir: "/tmp/p".into(),
            status: SessionStatus::Active,
            control: ControlModel::Tmux,
            active_delegations: 0,
        });
        (state, id)
    }

    #[tokio::test]
    async fn health_endpoint_responds() {
        assert_eq!(health().await, "ok");
    }

    #[tokio::test]
    async fn list_sessions_returns_state() {
        let (state, _) = state_with_session();
        let Json(body) = list_sessions(State(state)).await;
        assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn hook_relay_ingests_known_event() {
        let (state, id) = state_with_session();
        let post = HookPost {
            session_id: id.0.to_string(),
            event: "PostToolUse".into(),
            payload: serde_json::json!({"tool": "Edit"}),
        };
        let result = ingest_hook(State(state.clone()), Json(post)).await;
        assert!(result.is_ok());
        assert_eq!(state.recent_hook_events().len(), 1);
    }

    #[tokio::test]
    async fn hook_relay_rejects_unknown_event() {
        let (state, id) = state_with_session();
        let post = HookPost {
            session_id: id.0.to_string(),
            event: "Bogus".into(),
            payload: serde_json::Value::Null,
        };
        let err = ingest_hook(State(state), Json(post)).await.unwrap_err();
        assert_eq!(err, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn register_and_remove_session() {
        let state = DaemonState::shared();
        let Json(body) = register_session(
            State(state.clone()),
            Json(RegisterSession {
                workdir: "/tmp/new".into(),
            }),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_string();
        assert_eq!(state.list_sessions().len(), 1);
        // Removing it succeeds; removing again is a 404.
        assert!(
            remove_session(State(state.clone()), Path(id.clone()))
                .await
                .is_ok()
        );
        let err = remove_session(State(state), Path(id)).await.unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn hook_relay_rejects_bad_session_id() {
        let (state, _) = state_with_session();
        let post = HookPost {
            session_id: "not-a-uuid".into(),
            event: "Stop".into(),
            payload: serde_json::Value::Null,
        };
        let err = ingest_hook(State(state), Json(post)).await.unwrap_err();
        assert_eq!(err, StatusCode::BAD_REQUEST);
    }
}
