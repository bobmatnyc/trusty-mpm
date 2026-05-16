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

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde_json::Value;
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use trusty_mpm_core::hook::{HookEvent, HookEventRecord};
use trusty_mpm_core::overseer::{OverseerContext, OverseerDecision};
use trusty_mpm_core::project::ProjectInfo;
use trusty_mpm_core::session::{ControlModel, Session, SessionId, SessionStatus};
use trusty_mpm_core::tmux::TmuxTarget;

use crate::audit::AuditEntry;
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
        .route("/sessions/dead", axum::routing::delete(reap_sessions))
        .route("/sessions/{id}", axum::routing::delete(remove_session))
        .route("/sessions/{id}/events", get(session_events))
        .route("/projects", get(list_projects).post(register_project))
        .route("/projects/current", get(current_project))
        .route("/events", get(recent_events))
        .route("/hooks", post(ingest_hook))
        .route("/breakers", get(breakers))
        .route("/optimizer", get(get_optimizer))
        .route("/overseer", get(get_overseer))
        .merge(
            SwaggerUi::new("/api-docs")
                .url("/api-docs/openapi.json", crate::openapi::ApiDoc::openapi()),
        )
        .with_state(state)
}

/// Liveness probe — always returns `ok` while the daemon is up.
#[utoipa::path(
    get,
    path = "/health",
    tag = "config",
    responses((status = 200, description = "Daemon is alive", body = String))
)]
pub async fn health() -> &'static str {
    "ok"
}

/// Query parameters for `GET /sessions`.
///
/// Why: `trusty-mpm session list` scopes the listing to one project; an
/// optional `?project=<path>` filter keeps the endpoint usable both ways.
/// What: an optional project path; when absent, all sessions are returned.
/// Test: `list_sessions_filters_by_project`.
#[derive(serde::Deserialize, Default)]
pub struct SessionQuery {
    /// Optional project path to filter sessions by.
    pub project: Option<PathBuf>,
}

/// `GET /sessions` — snapshot of managed sessions, optionally project-scoped.
#[utoipa::path(
    get,
    path = "/sessions",
    tag = "sessions",
    params(("project" = Option<String>, Query, description = "Filter by project path")),
    responses((status = 200, description = "Array of managed sessions", body = [Session]))
)]
pub async fn list_sessions(
    State(state): State<Arc<DaemonState>>,
    Query(query): Query<SessionQuery>,
) -> Json<serde_json::Value> {
    let sessions = match query.project {
        Some(path) => state.list_sessions_for_project(&path),
        None => state.list_sessions(),
    };
    Json(serde_json::json!({ "sessions": sessions }))
}

/// `GET /events` — recent hook events across all sessions (dashboard feed).
#[utoipa::path(
    get,
    path = "/events",
    tag = "events",
    responses((status = 200, description = "Recent hook events across all sessions"))
)]
pub async fn recent_events(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "events": state.recent_hook_events() }))
}

/// JSON body for registering a session via `POST /sessions`.
///
/// Why: a session created by an external launcher (or the CLI) must announce
/// itself so the dashboard and MCP tools can see it.
/// What: the working directory the session runs in.
/// Test: `register_and_remove_session`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct RegisterSession {
    /// Working directory the session was launched in.
    pub workdir: String,
    /// Optional project this session belongs to. When present, the session is
    /// associated with that registered project so `session list` can scope to
    /// it.
    #[serde(default)]
    #[schema(value_type = Option<String>)]
    pub project_path: Option<PathBuf>,
}

/// `POST /sessions` — register a new managed session, returning its id.
///
/// Why: registering a session should also stand up its tmux host so the
/// operator gets a live Claude Code session, not just a bookkeeping entry.
/// What: builds the `Session`, then best-effort launches a detached tmux
/// session and starts `claude` in it; tmux failures are logged, not fatal —
/// the session is still registered so the API stays usable without tmux.
/// Test: `register_and_remove_session` covers the bookkeeping path.
#[utoipa::path(
    post,
    path = "/sessions",
    tag = "sessions",
    request_body = RegisterSession,
    responses((status = 201, description = "Session registered; returns its id and name"))
)]
pub async fn register_session(
    State(state): State<Arc<DaemonState>>,
    Json(body): Json<RegisterSession>,
) -> Json<serde_json::Value> {
    let mut session = Session::new(SessionId::new(), body.workdir.clone(), ControlModel::Tmux);
    session.project_path = body.project_path.clone();
    let id = session.id;
    let tmux_name = session.tmux_name.clone();
    state.register_session(session);

    // Best-effort: launch the session inside tmux. Any failure is logged and
    // the session stays registered in the `Starting` state. The session is
    // hosted under a friendly, deterministic name derived from its UUID.
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

    Json(serde_json::json!({ "id": id, "name": tmux_name }))
}

/// `DELETE /sessions/:id` — deregister a session.
#[utoipa::path(
    delete,
    path = "/sessions/{id}",
    tag = "sessions",
    params(("id" = String, Path, description = "Session UUID")),
    responses(
        (status = 200, description = "Session removed"),
        (status = 404, description = "No session with that id"),
    )
)]
pub async fn remove_session(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = parse_id(&id)?;
    match state.remove_session(session) {
        Some(_) => Ok(Json(serde_json::json!({ "removed": id }))),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// `DELETE /sessions/dead` — reap registry entries with no live tmux session.
///
/// Why: dead sessions accumulate forever otherwise; an operator (or a periodic
/// task) needs a way to prune the registry down to what tmux actually hosts.
/// What: discovers tmux, calls [`DaemonState::reap_dead_sessions`], and returns
/// `{ "removed": <count> }`. If tmux is unavailable nothing is reaped (returns
/// `0`) — reaping against an empty list would wrongly delete every session.
/// Test: `reap_dead_sessions` in `state.rs` covers the core logic.
#[utoipa::path(
    delete,
    path = "/sessions/dead",
    tag = "sessions",
    responses((status = 200, description = "Dead sessions reaped; returns the removed count"))
)]
pub async fn reap_sessions(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let removed = match TmuxDriver::discover() {
        Ok(driver) => state.reap_dead_sessions(&driver),
        Err(_) => {
            tracing::info!("tmux unavailable; skipping dead-session reap");
            0
        }
    };
    Json(serde_json::json!({ "removed": removed }))
}

/// `GET /sessions/:id/events` — recent hook events for one session.
#[utoipa::path(
    get,
    path = "/sessions/{id}/events",
    tag = "events",
    params(("id" = String, Path, description = "Session UUID")),
    responses(
        (status = 200, description = "Recent hook events for the session"),
        (status = 404, description = "No session with that id"),
    )
)]
pub async fn session_events(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = parse_id(&id)?;
    Ok(Json(serde_json::json!({
        "events": state.hook_events_for(session),
    })))
}

/// `GET /breakers` — every agent's circuit-breaker state.
#[utoipa::path(
    get,
    path = "/breakers",
    tag = "config",
    responses((status = 200, description = "Array of per-agent circuit-breaker states"))
)]
pub async fn breakers(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
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
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct HookPost {
    /// Session the event came from (UUID string).
    pub session_id: String,
    /// Claude Code event name, e.g. `PreToolUse`.
    pub event: String,
    /// Raw event payload (shape varies per event).
    #[serde(default)]
    #[schema(value_type = Object)]
    pub payload: serde_json::Value,
}

/// Build an [`OverseerContext`] from a hook payload.
///
/// Why: the overseer evaluates events by tool name and input; extracting these
/// from the opaque payload in one place keeps the relay handler readable.
/// What: resolves the session's friendly tmux name (falling back to the UUID),
/// reads `payload["tool"]` as the tool name, and serializes `payload["input"]`
/// (or the whole payload when absent) as the tool input.
fn overseer_context(state: &DaemonState, session: SessionId, payload: &Value) -> OverseerContext {
    let tmux_name = state
        .session(session)
        .map(|s| s.tmux_name)
        .unwrap_or_else(|| session.0.to_string());
    let tool_name = payload
        .get("tool")
        .and_then(Value::as_str)
        .map(str::to_string);
    let tool_input = payload
        .get("input")
        .map(|v| v.to_string())
        .or_else(|| Some(payload.to_string()));
    OverseerContext::new(session, tmux_name, tool_name, tool_input)
}

/// `POST /hooks` — universal hook relay; ingests one Claude Code hook event.
///
/// Why: this is how the daemon achieves full observability — a forwarder shim
/// configured for *all* 32 hook events posts each one here. It is also the
/// enforcement point for the optional session overseer.
/// What: parses the session id and event name, runs the overseer on tool-use
/// events (auditing every decision; a `Block` returns `403` early), compresses
/// `PostToolUse` output, then appends a `HookEventRecord` to the ring buffer.
/// Rejects unknown events/ids with `400`.
/// Test: `hook_relay_ingests_known_event`, `hook_relay_rejects_unknown_event`,
/// `overseer_blocks_pre_tool_use`.
#[utoipa::path(
    post,
    path = "/hooks",
    tag = "internal",
    request_body = HookPost,
    responses(
        (status = 200, description = "Hook event accepted"),
        (status = 400, description = "Unknown event name or malformed session id"),
        (status = 403, description = "Overseer blocked the event"),
    )
)]
pub async fn ingest_hook(
    State(state): State<Arc<DaemonState>>,
    Json(post): Json<HookPost>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = parse_id(&post.session_id)?;
    let event = HookEvent::from_wire(&post.event).ok_or(StatusCode::BAD_REQUEST)?;

    let mut payload = post.payload;

    // Overseer: evaluate and audit tool-use events. Skipped entirely when the
    // overseer is disabled (the common, opt-out path).
    let overseer = state.overseer();
    if overseer.is_enabled()
        && let Some(decision) = run_overseer(&state, &overseer, event, session, &payload)
    {
        // A Block verdict halts the event: return early with `403` so the
        // forwarder shim can surface the refusal to Claude Code.
        if matches!(decision, OverseerDecision::Block { .. }) {
            return Err(StatusCode::FORBIDDEN);
        }
        // Respond verdicts are noted for the (future) tmux send-keys wiring.
        if let OverseerDecision::Respond { text } = &decision {
            tracing::info!("overseer auto-response for {session:?}: {text}");
        }
    }

    // For PostToolUse events, compress the tool output before it enters the
    // ring buffer (and hence the dashboard / compacted history).
    if event == HookEvent::PostToolUse {
        let tool_name = payload
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let cfg = state.optimizer_config();
        crate::optimizer::optimize_tool_output(&cfg, &tool_name, &mut payload);
    }

    state.push_hook_event(HookEventRecord::now(session, event, payload));
    Ok(Json(serde_json::json!({ "accepted": post.event })))
}

/// Run the overseer for one hook event and audit the verdict.
///
/// Why: keeping the event-kind dispatch and the audit write in one helper keeps
/// [`ingest_hook`] focused on the relay flow.
/// What: maps `PreToolUse` / `PostToolUse` / `Stop` events onto the matching
/// overseer call, writes an [`AuditEntry`], and returns the decision. Events
/// the overseer does not act on return `None`.
fn run_overseer(
    state: &DaemonState,
    overseer: &Arc<dyn trusty_mpm_core::overseer::Overseer>,
    event: HookEvent,
    session: SessionId,
    payload: &Value,
) -> Option<OverseerDecision> {
    let ctx = overseer_context(state, session, payload);
    let (event_label, decision) = match event {
        HookEvent::PreToolUse => ("PreToolUse", overseer.pre_tool_use(&ctx)),
        HookEvent::PostToolUse => {
            let output = payload.get("output").and_then(Value::as_str).unwrap_or("");
            ("PostToolUse", overseer.post_tool_use(&ctx, output))
        }
        _ => return None,
    };
    state.audit().log(AuditEntry::from_decision(
        &ctx,
        event_label,
        &decision,
        "deterministic",
    ));
    Some(decision)
}

/// `GET /overseer` — current session-overseer configuration and status.
///
/// Why: the CLI and dashboard surface whether oversight is active and which
/// strategy is in force.
/// What: returns `{ "overseer": { "enabled": <bool>, "handler": "deterministic" } }`.
/// Test: `get_overseer_returns_status`.
#[utoipa::path(
    get,
    path = "/overseer",
    tag = "config",
    responses((status = 200, description = "Overseer enabled flag and handler type"))
)]
pub async fn get_overseer(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "overseer": {
            "enabled": state.overseer().is_enabled(),
            "handler": "deterministic",
        }
    }))
}

/// `GET /optimizer` — current token-use optimizer configuration.
///
/// Why: the CLI and dashboard surface the active compression tuning. The
/// config is now framework-managed on disk (`optimizer.toml`); this endpoint
/// is read-only introspection of the daemon's in-memory copy of it.
/// What: returns `{ "optimizer": <OptimizerConfig> }`.
/// Test: `get_optimizer_returns_default`.
#[utoipa::path(
    get,
    path = "/optimizer",
    tag = "config",
    responses((status = 200, description = "Current token-use optimizer configuration"))
)]
pub async fn get_optimizer(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "optimizer": state.optimizer_config() }))
}

/// JSON body for registering a project via `POST /projects`.
///
/// Why: `trusty-mpm project init` announces a working directory to the daemon
/// so sessions started there can be associated with it.
/// What: the absolute path of the project's working directory.
/// Test: `register_and_list_projects`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct RegisterProject {
    /// Absolute path to the project's working directory.
    #[schema(value_type = String)]
    pub path: PathBuf,
}

/// `POST /projects` — register a project, returning its `ProjectInfo`.
///
/// Why: the daemon owns the project registry; `project init` posts the
/// resolved directory here.
/// What: delegates to [`DaemonState::register_project`] and returns the
/// stored info as JSON.
/// Test: `register_and_list_projects`.
#[utoipa::path(
    post,
    path = "/projects",
    tag = "projects",
    request_body = RegisterProject,
    responses((status = 201, description = "Project registered", body = ProjectInfo))
)]
pub async fn register_project(
    State(state): State<Arc<DaemonState>>,
    Json(body): Json<RegisterProject>,
) -> Json<serde_json::Value> {
    let info = state.register_project(body.path);
    Json(serde_json::json!(info))
}

/// `GET /projects` — snapshot of every registered project.
#[utoipa::path(
    get,
    path = "/projects",
    tag = "projects",
    responses((status = 200, description = "Array of registered projects", body = [ProjectInfo]))
)]
pub async fn list_projects(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "projects": state.list_projects() }))
}

/// Query parameters for `GET /projects/current`.
///
/// Why: the daemon cannot see the caller's cwd; the CLI passes the resolved
/// path so the daemon can look the project up.
/// What: the path to resolve a project for.
/// Test: `current_project_found_and_missing`.
#[derive(serde::Deserialize)]
pub struct CurrentProjectQuery {
    /// Path whose registered project should be returned.
    pub path: PathBuf,
}

/// `GET /projects/current?path=<dir>` — the project registered for `path`.
///
/// Why: `trusty-mpm project info` shows the current directory's project; the
/// daemon resolves the path against its registry.
/// What: returns the matching `ProjectInfo`, or `404` when `path` is not a
/// registered project.
/// Test: `current_project_found_and_missing`.
#[utoipa::path(
    get,
    path = "/projects/current",
    tag = "projects",
    params(("path" = String, Query, description = "Directory whose project to resolve")),
    responses(
        (status = 200, description = "The project registered for the path", body = ProjectInfo),
        (status = 404, description = "Path is not a registered project"),
    )
)]
pub async fn current_project(
    State(state): State<Arc<DaemonState>>,
    Query(query): Query<CurrentProjectQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match state.project(&query.path) {
        Some(info) => Ok(Json(serde_json::json!(info))),
        None => Err(StatusCode::NOT_FOUND),
    }
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
        let mut session = Session::new(id, "/tmp/p", ControlModel::Tmux);
        session.status = SessionStatus::Active;
        state.register_session(session);
        (state, id)
    }

    #[tokio::test]
    async fn health_endpoint_responds() {
        assert_eq!(health().await, "ok");
    }

    #[tokio::test]
    async fn list_sessions_returns_state() {
        let (state, _) = state_with_session();
        let Json(body) = list_sessions(State(state), Query(SessionQuery::default())).await;
        assert_eq!(body["sessions"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn register_and_list_projects() {
        // `POST /projects` registers a project; `GET /projects` lists it.
        let state = DaemonState::shared();
        let Json(info) = register_project(
            State(Arc::clone(&state)),
            Json(RegisterProject {
                path: "/work/demo".into(),
            }),
        )
        .await;
        assert_eq!(info["name"], "demo");
        assert_eq!(info["path"], "/work/demo");

        let Json(body) = list_projects(State(state)).await;
        assert_eq!(body["projects"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn current_project_found_and_missing() {
        // `GET /projects/current` returns the project for a registered path
        // and `404` for an unregistered one.
        let state = DaemonState::shared();
        let _ = register_project(
            State(Arc::clone(&state)),
            Json(RegisterProject {
                path: "/work/demo".into(),
            }),
        )
        .await;

        let ok = current_project(
            State(Arc::clone(&state)),
            Query(CurrentProjectQuery {
                path: "/work/demo".into(),
            }),
        )
        .await;
        assert!(ok.is_ok());

        let err = current_project(
            State(state),
            Query(CurrentProjectQuery {
                path: "/work/missing".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn register_session_associates_project() {
        // A `POST /sessions` body carrying `project_path` must associate the
        // new session with that project.
        let state = DaemonState::shared();
        let Json(body) = register_session(
            State(Arc::clone(&state)),
            Json(RegisterSession {
                workdir: "/work/demo".into(),
                project_path: Some("/work/demo".into()),
            }),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_string();
        let listed = state.list_sessions();
        let session = listed
            .iter()
            .find(|s| s.id.0.to_string() == id)
            .expect("session registered");
        assert_eq!(session.project_path, Some(PathBuf::from("/work/demo")));
    }

    #[tokio::test]
    async fn list_sessions_filters_by_project() {
        // `GET /sessions?project=<path>` returns only sessions of that project.
        let state = DaemonState::shared();
        let _ = register_session(
            State(Arc::clone(&state)),
            Json(RegisterSession {
                workdir: "/work/demo".into(),
                project_path: Some("/work/demo".into()),
            }),
        )
        .await;
        let _ = register_session(
            State(Arc::clone(&state)),
            Json(RegisterSession {
                workdir: "/work/other".into(),
                project_path: Some("/work/other".into()),
            }),
        )
        .await;

        let Json(all) =
            list_sessions(State(Arc::clone(&state)), Query(SessionQuery::default())).await;
        assert_eq!(all["sessions"].as_array().unwrap().len(), 2);

        let Json(scoped) = list_sessions(
            State(state),
            Query(SessionQuery {
                project: Some("/work/demo".into()),
            }),
        )
        .await;
        assert_eq!(scoped["sessions"].as_array().unwrap().len(), 1);
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
                project_path: None,
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
    async fn registered_session_has_friendly_tmux_name() {
        // A registered session must carry a `tmpm-<adj>-<noun>` tmux name
        // derived from its UUID, not the legacy `trusty-mpm-<uuid>` form.
        let state = DaemonState::shared();
        let Json(body) = register_session(
            State(Arc::clone(&state)),
            Json(RegisterSession {
                workdir: "/tmp/friendly".into(),
                project_path: None,
            }),
        )
        .await;
        let id = body["id"].as_str().unwrap().to_string();
        let listed = state.list_sessions();
        let session = listed
            .iter()
            .find(|s| s.id.0.to_string() == id)
            .expect("session registered");
        assert!(
            session.tmux_name.starts_with("tmpm-"),
            "friendly name: {}",
            session.tmux_name
        );
        assert!(session.tmux_name.len() <= 25);
    }

    #[tokio::test]
    async fn reap_sessions_returns_removed_count() {
        // `DELETE /sessions/dead` always returns a well-formed `{ "removed": N }`
        // body. The exact count depends on whether tmux is installed: with tmux
        // the lone test session (no live tmux session named `tmpm-*`) is reaped
        // (1); without tmux nothing is reaped (0). Either way the registry must
        // not contain a session that is missing from tmux afterwards.
        let (state, _) = state_with_session();
        let Json(body) = reap_sessions(State(Arc::clone(&state))).await;
        let removed = body["removed"].as_u64().expect("removed is a number");
        assert!(removed <= 1, "at most the one test session is reaped");
        assert_eq!(state.list_sessions().len() as u64, 1 - removed);
    }

    #[tokio::test]
    async fn register_session_returns_id_even_without_tmux() {
        // Graceful-degradation invariant: tmux is unavailable in CI, yet
        // `POST /sessions` must still return a JSON body carrying an `id`, and
        // that id must be visible in the subsequent `GET /sessions` snapshot.
        let state = DaemonState::shared();
        let Json(body) = register_session(
            State(Arc::clone(&state)),
            Json(RegisterSession {
                workdir: "/tmp/no-tmux".into(),
                project_path: None,
            }),
        )
        .await;
        let id_str = body
            .get("id")
            .and_then(|v| v.as_str())
            .expect("response body must contain an `id` string");
        let listed = state.list_sessions();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.0.to_string(), id_str);
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

    #[tokio::test]
    async fn get_overseer_returns_status() {
        // `GET /overseer` must return 200 with the enabled flag and handler.
        // With no framework installed the overseer is disabled by default.
        let state = DaemonState::shared();
        let Json(body) = get_overseer(State(state)).await;
        assert_eq!(body["overseer"]["enabled"], false);
        assert_eq!(body["overseer"]["handler"], "deterministic");
    }

    #[tokio::test]
    async fn hook_relay_runs_with_disabled_overseer() {
        // With the overseer disabled (the default), a PreToolUse event must
        // still be ingested normally — the overseer fast-path allows it.
        let (state, id) = state_with_session();
        let post = HookPost {
            session_id: id.0.to_string(),
            event: "PreToolUse".into(),
            payload: serde_json::json!({"tool": "Bash", "input": {"command": "ls"}}),
        };
        let result = ingest_hook(State(state.clone()), Json(post)).await;
        assert!(result.is_ok());
        assert_eq!(state.recent_hook_events().len(), 1);
    }

    #[tokio::test]
    async fn session_events_returns_empty_initially() {
        // A freshly-registered session has no hook events; `GET
        // /sessions/{id}/events` must return 200 with an empty array.
        let (state, id) = state_with_session();
        let result = session_events(State(state), Path(id.0.to_string())).await;
        let Json(body) = result.expect("valid session id resolves");
        assert_eq!(body["events"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn recent_events_returns_ring_buffer() {
        // A valid hook event posted via `POST /hooks` must appear in the
        // `GET /events` ring-buffer feed.
        let (state, id) = state_with_session();
        let post = HookPost {
            session_id: id.0.to_string(),
            event: "PreToolUse".into(),
            payload: serde_json::json!({"tool": "Read"}),
        };
        let _ = ingest_hook(State(Arc::clone(&state)), Json(post))
            .await
            .expect("known event ingests");

        let Json(body) = recent_events(State(state)).await;
        let events = body["events"].as_array().expect("events is an array");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["session"], id.0.to_string());
        assert_eq!(events[0]["event"], "PreToolUse");
    }

    #[tokio::test]
    async fn openapi_spec_is_valid() {
        // `GET /api-docs/openapi.json` must return 200 with a document that
        // carries the `openapi` version key and the daemon's title.
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let app = router(DaemonState::shared());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api-docs/openapi.json")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let spec: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            spec.get("openapi").is_some(),
            "spec must have an openapi key"
        );
        assert!(
            spec["info"]["title"]
                .as_str()
                .unwrap_or_default()
                .contains("trusty-mpm"),
            "spec title must mention trusty-mpm"
        );
    }

    #[tokio::test]
    async fn breakers_endpoint_returns_200() {
        // `GET /breakers` must return 200 with a well-formed `breakers` array,
        // even when no breakers have been created yet.
        let state = DaemonState::shared();
        let Json(body) = breakers(State(state)).await;
        assert!(body["breakers"].is_array());
    }

    #[tokio::test]
    async fn get_optimizer_returns_default() {
        let state = DaemonState::shared();
        let Json(body) = get_optimizer(State(state)).await;
        assert_eq!(body["optimizer"]["default_level"], "trim");
        assert_eq!(body["optimizer"]["suppress_redundant_reads"], true);
    }
}
