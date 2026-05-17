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

use trusty_mpm_core::compress::{CompressionLevel, compress_output};
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
        .route("/sessions/{id}/pause", post(pause_session))
        .route("/sessions/{id}/resume", post(resume_session))
        .route("/sessions/{id}/command", post(send_command))
        .route("/sessions/{id}/output", get(get_output))
        .route("/projects", get(list_projects).post(register_project))
        .route("/projects/current", get(current_project))
        .route("/events", get(recent_events))
        .route("/hooks", post(ingest_hook))
        .route("/breakers", get(breakers))
        .route("/optimizer", get(get_optimizer))
        .route("/overseer", get(get_overseer))
        .route("/tmux/sessions", get(list_tmux_sessions))
        .route("/tmux/sessions/{name}/snapshot", get(tmux_snapshot))
        .route("/tmux/adopt", post(adopt_tmux_session))
        .route("/claude-config", get(get_claude_config))
        .route("/claude-config/apply", post(apply_claude_config))
        .route("/claude-config/restart", post(restart_claude_code))
        .route(
            "/claude-config/checkpoints",
            get(list_checkpoints).post(create_checkpoint),
        )
        .route(
            "/claude-config/checkpoints/{id}",
            axum::routing::delete(delete_checkpoint),
        )
        .route("/claude-config/restore", post(restore_checkpoint))
        .route("/claude-config/profiles", get(list_profiles))
        .route("/claude-config/deploy", post(deploy_profile))
        .route("/pair/request", post(pair_request))
        .route("/pair/confirm", post(pair_confirm))
        .route("/pair/status", get(pair_status))
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

/// Resolve a `{id}` path param against the session registry.
///
/// Why: the pause/resume/command/output endpoints accept either a session UUID
/// or its friendly `tmpm-<adj>-<noun>` tmux name, exactly like the CLI's
/// `session stop`. Centralizing the lookup keeps the four handlers uniform.
/// What: tries `DaemonState::find_session`, which matches a UUID string first
/// and then scans for a matching `tmux_name`; maps a miss to `404`.
fn resolve_session(state: &DaemonState, key: &str) -> Result<Session, StatusCode> {
    state.find_session(key).ok_or(StatusCode::NOT_FOUND)
}

/// Best-effort capture of a session's tmux pane output.
///
/// Why: pause/command/output all want recent pane text, but tmux may be absent
/// (CI) or the session may not be hosted in tmux. None of those is fatal — the
/// endpoints still succeed, just without captured text.
/// What: discovers tmux and captures the last `lines` of the session's pane;
/// any failure is logged and yields an empty string.
fn capture_pane(session: &Session, lines: u32) -> String {
    match TmuxDriver::discover() {
        Ok(driver) => {
            let target = TmuxTarget::session(&session.tmux_name);
            match driver.capture(&target, Some(lines)) {
                Ok(text) => text,
                Err(e) => {
                    tracing::warn!("tmux capture failed for {}: {e}", session.tmux_name);
                    String::new()
                }
            }
        }
        Err(_) => {
            tracing::info!(
                "tmux unavailable; capture for {} skipped",
                session.tmux_name
            );
            String::new()
        }
    }
}

/// Result of applying an optional compression level to captured output.
///
/// Why: the command and output endpoints share the same compress-then-return
/// shape; bundling the text and stats lets one helper produce both.
/// What: the (possibly compressed) text, the byte stats, and the level as a
/// lowercase wire string (`None` when no compression was applied).
/// Test: `apply_compression_off_is_passthrough`, `apply_compression_summarise`.
struct CompressedOutput {
    /// The output text after compression (or unchanged when off).
    text: String,
    /// Byte counts before and after compression.
    stats: trusty_mpm_core::compress::CompressionStats,
    /// Lowercase wire name of the level applied, or `None` when uncompressed.
    level_label: Option<String>,
}

/// Apply an optional compression level to captured pane output.
///
/// Why: `POST .../command` and `GET .../output` both accept an optional
/// `?compress=` query param; doing the compress-or-passthrough decision once
/// keeps the two handlers identical.
/// What: when `level` is `Some`, runs [`compress_output`] and records the
/// level's lowercase label; when `None`, returns the raw text with empty stats
/// and no label.
/// Test: `apply_compression_off_is_passthrough`, `apply_compression_summarise`.
fn apply_compression(level: Option<CompressionLevel>, raw: &str) -> CompressedOutput {
    match level {
        Some(level) => {
            let (text, stats) = compress_output(raw, level);
            CompressedOutput {
                text,
                stats,
                level_label: Some(compression_level_label(level)),
            }
        }
        None => CompressedOutput {
            text: raw.to_string(),
            stats: trusty_mpm_core::compress::CompressionStats::default(),
            level_label: None,
        },
    }
}

/// Lowercase wire name for a [`CompressionLevel`].
///
/// Why: API responses report the applied level as a stable lowercase string,
/// matching the `snake_case` serde representation of the enum.
/// What: maps each variant to its `serde` wire name.
/// Test: `compress_level_label_matches_serde`.
fn compression_level_label(level: CompressionLevel) -> String {
    match level {
        CompressionLevel::Off => "off",
        CompressionLevel::Trim => "trim",
        CompressionLevel::Summarise => "summarise",
        CompressionLevel::Caveman => "caveman",
    }
    .to_string()
}

/// JSON body for `POST /sessions/{id}/pause`.
///
/// Why: a pause may carry an optional operator note describing where the
/// session was left off; when absent the daemon derives one from pane output.
/// What: an optional free-form summary string.
/// Test: `pause_then_resume_round_trips`.
#[derive(serde::Deserialize, utoipa::ToSchema, Default)]
pub struct PauseRequest {
    /// Optional note about where the session was left off.
    #[serde(default)]
    pub summary: Option<String>,
}

/// `POST /sessions/{id}/pause` — pause a session, saving its state for resume.
///
/// Why: an operator stepping away needs the session frozen with a "where I left
/// off" note that survives a daemon restart.
/// What: resolves the session by UUID or friendly name, captures the last 50
/// pane lines, sets `status = Paused` / `paused_at = now` / `pause_summary`
/// (the request note, or the first 500 chars of the `Summarise`-compressed
/// captured output), and mirrors the pause record to disk via
/// `session_store::save_pause`.
/// Test: `pause_then_resume_round_trips`, `pause_unknown_session_is_404`.
#[utoipa::path(
    post,
    path = "/sessions/{id}/pause",
    tag = "sessions",
    params(("id" = String, Path, description = "Session UUID or friendly name")),
    request_body = PauseRequest,
    responses(
        (status = 200, description = "Session paused; returns the pause summary"),
        (status = 404, description = "No session with that id or name"),
    )
)]
pub async fn pause_session(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    Json(body): Json<PauseRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = resolve_session(&state, &id)?;
    let captured = capture_pane(&session, 50);
    let summary = body.summary.unwrap_or_else(|| {
        // No operator note: derive a cleaner auto-summary by running the
        // captured pane text through `Summarise` compression (strips ANSI,
        // collapses blank lines) and keeping the first 500 chars.
        let (compressed, _) = compress_output(&captured, CompressionLevel::Summarise);
        compressed.chars().take(500).collect::<String>()
    });
    let now = std::time::SystemTime::now();

    state.update_session(&session.id, |s| {
        s.status = SessionStatus::Paused;
        s.paused_at = Some(now);
        s.pause_summary = Some(summary.clone());
    });

    // Persist the pause record so the state survives a daemon restart.
    if let Some(updated) = state.session(session.id)
        && let Err(e) = trusty_mpm_core::session_store::save_pause(&updated)
    {
        tracing::warn!(
            "failed to persist pause state for {}: {e}",
            session.tmux_name
        );
    }

    Ok(Json(serde_json::json!({
        "paused": true,
        "session_id": session.id,
        "summary": summary,
    })))
}

/// `POST /sessions/{id}/resume` — resume a previously-paused session.
///
/// Why: the counterpart to pause; clears the frozen state and the on-disk
/// pause record so the session is active again.
/// What: resolves the session, requires `status == Paused` (else `409`), sets
/// `status = Active` / `paused_at = None` / `pause_summary = None`, and removes
/// the pause file via `session_store::clear_pause`.
/// Test: `pause_then_resume_round_trips`, `resume_unpaused_session_is_409`.
#[utoipa::path(
    post,
    path = "/sessions/{id}/resume",
    tag = "sessions",
    params(("id" = String, Path, description = "Session UUID or friendly name")),
    responses(
        (status = 200, description = "Session resumed"),
        (status = 404, description = "No session with that id or name"),
        (status = 409, description = "Session is not paused"),
    )
)]
pub async fn resume_session(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = resolve_session(&state, &id)?;
    if session.status != SessionStatus::Paused {
        return Err(StatusCode::CONFLICT);
    }

    state.update_session(&session.id, |s| {
        s.status = SessionStatus::Active;
        s.paused_at = None;
        s.pause_summary = None;
    });

    if let Err(e) = trusty_mpm_core::session_store::clear_pause(&session.id) {
        tracing::warn!("failed to clear pause state for {}: {e}", session.tmux_name);
    }

    Ok(Json(serde_json::json!({ "resumed": true })))
}

/// JSON body for `POST /sessions/{id}/command`.
///
/// Why: feeding a command into a session's tmux pane is how the operator (and
/// the Telegram bot) drives Claude Code remotely.
/// What: the command line to type into the pane.
/// Test: `send_command_returns_output_shape`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct CommandRequest {
    /// The command line to send to the session's tmux pane.
    pub command: String,
}

/// Query parameters for `POST /sessions/{id}/command`.
///
/// Why: the caller may want the captured output summarised before it returns,
/// completing the "summarize output" step of the full user cycle.
/// What: an optional compression level (`off`, `trim`, `summarise`,
/// `caveman`); when absent the raw pane capture is returned unchanged.
/// Test: `send_command_compress_query_defaults_off`.
#[derive(serde::Deserialize, Default)]
pub struct CommandQuery {
    /// Compression level to apply to the captured output before returning.
    /// Values: off, trim, summarise, caveman. Defaults to none (raw output).
    #[serde(default)]
    pub compress: Option<CompressionLevel>,
}

/// `POST /sessions/{id}/command` — send a command to a session's tmux pane.
///
/// Why: remote control of a running session — type a line, let it run, read
/// back what happened.
/// What: resolves the session (`404` if missing, `409` if `Stopped`), sends the
/// command via `TmuxDriver::send_line`, waits 500ms for output to settle, then
/// captures the last 100 pane lines. When `?compress=` is supplied the capture
/// is compressed at that level before returning. tmux errors are logged, not
/// fatal — the endpoint still returns `200` with whatever output was captured.
/// Test: `send_command_returns_output_shape`, `command_to_stopped_session_is_409`.
#[utoipa::path(
    post,
    path = "/sessions/{id}/command",
    tag = "sessions",
    params(
        ("id" = String, Path, description = "Session UUID or friendly name"),
        ("compress" = Option<String>, Query, description = "Compression level: off, trim, summarise, caveman"),
    ),
    request_body = CommandRequest,
    responses(
        (status = 200, description = "Command sent; returns captured pane output"),
        (status = 404, description = "No session with that id or name"),
        (status = 409, description = "Session is stopped"),
    )
)]
pub async fn send_command(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    Query(query): Query<CommandQuery>,
    Json(body): Json<CommandRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = resolve_session(&state, &id)?;
    if session.status == SessionStatus::Stopped {
        return Err(StatusCode::CONFLICT);
    }

    // Best-effort: send the command. tmux may be absent in CI.
    match TmuxDriver::discover() {
        Ok(driver) => {
            let target = TmuxTarget::session(&session.tmux_name);
            if let Err(e) = driver.send_line(&target, &body.command) {
                tracing::warn!("tmux send_line failed for {}: {e}", session.tmux_name);
            }
        }
        Err(_) => {
            tracing::info!(
                "tmux unavailable; command for {} not sent",
                session.tmux_name
            );
        }
    }

    // Give the pane a moment to render the command's output before capturing.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let raw = capture_pane(&session, 100);
    let compressed = apply_compression(query.compress, &raw);

    Ok(Json(serde_json::json!({
        "sent": true,
        "output": compressed.text,
        "original_bytes": compressed.stats.original_bytes,
        "compressed_bytes": compressed.stats.compressed_bytes,
        "compress_level": compressed.level_label,
    })))
}

/// Query parameters for `GET /sessions/{id}/output`.
///
/// Why: the caller chooses how much scrollback to capture and whether to
/// summarise it; defaults keep the endpoint usable with no query string.
/// What: an optional line count (defaulting to 50 when absent) and an optional
/// compression level applied to the capture before returning.
/// Test: `get_output_returns_output_shape`, `output_query_defaults`.
#[derive(serde::Deserialize, Default)]
pub struct OutputQuery {
    /// Number of trailing pane lines to capture (default 50 when absent).
    #[serde(default)]
    pub lines: Option<u32>,
    /// Compression level to apply to the captured output before returning.
    /// Values: off, trim, summarise, caveman. Defaults to none (raw output).
    #[serde(default)]
    pub compress: Option<CompressionLevel>,
}

/// Default trailing-line count for `GET /sessions/{id}/output`.
fn default_output_lines() -> u32 {
    50
}

/// `GET /sessions/{id}/output` — capture the current tmux pane output.
///
/// Why: the dashboard and the Telegram bot show a session's recent output
/// without sending it a command.
/// What: resolves the session (`404` if missing), captures the last `?lines=N`
/// pane lines (default 50), optionally compresses it at `?compress=`, and
/// returns `{ output, lines, original_bytes, compressed_bytes, compress_level }`.
/// tmux being unavailable yields an empty `output` rather than an error.
/// Test: `get_output_returns_output_shape`, `output_unknown_session_is_404`.
#[utoipa::path(
    get,
    path = "/sessions/{id}/output",
    tag = "sessions",
    params(
        ("id" = String, Path, description = "Session UUID or friendly name"),
        ("lines" = Option<u32>, Query, description = "Trailing lines to capture (default 50)"),
        ("compress" = Option<String>, Query, description = "Compression level: off, trim, summarise, caveman"),
    ),
    responses(
        (status = 200, description = "Captured pane output"),
        (status = 404, description = "No session with that id or name"),
    )
)]
pub async fn get_output(
    State(state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    Query(query): Query<OutputQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let session = resolve_session(&state, &id)?;
    let lines = query.lines.unwrap_or_else(default_output_lines);
    let raw = capture_pane(&session, lines);
    let compressed = apply_compression(query.compress, &raw);
    Ok(Json(serde_json::json!({
        "output": compressed.text,
        "lines": lines,
        "original_bytes": compressed.stats.original_bytes,
        "compressed_bytes": compressed.stats.compressed_bytes,
        "compress_level": compressed.level_label,
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
        state.overseer_handler(),
    ));
    Some(decision)
}

/// `GET /overseer` — current session-overseer configuration and status.
///
/// Why: the CLI and dashboard surface whether oversight is active and which
/// strategy is in force.
/// What: returns `{ "overseer": { "enabled": <bool>, "handler": <str> } }`,
/// where `handler` is the active strategy name reported by the overseer.
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
            "handler": state.overseer_handler(),
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

// ---- universal tmux session management ---------------------------------

/// `GET /tmux/sessions` — every tmux session on the host, origin-tagged.
///
/// Why: trusty-mpm manages *all* tmux sessions, not just the ones it created;
/// the dashboard needs the full list with an origin label so it can offer to
/// adopt external sessions.
/// What: runs `TmuxDriver::list_all_sessions` and returns
/// `{ "sessions": [ExternalSession, ...] }`. tmux being unavailable yields an
/// empty array rather than an error.
/// Test: `list_tmux_sessions_returns_array`.
#[utoipa::path(
    get,
    path = "/tmux/sessions",
    tag = "tmux",
    responses((status = 200, description = "All tmux sessions with origin labels"))
)]
pub async fn list_tmux_sessions(State(_state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let sessions = match TmuxDriver::discover() {
        Ok(driver) => driver.list_all_sessions().unwrap_or_else(|e| {
            tracing::warn!("tmux list_all_sessions failed: {e}");
            Vec::new()
        }),
        Err(_) => {
            tracing::info!("tmux unavailable; /tmux/sessions returns empty");
            Vec::new()
        }
    };
    Json(serde_json::json!({ "sessions": sessions }))
}

/// `GET /tmux/sessions/{name}/snapshot` — capture any session's current state.
///
/// Why: the dashboard inspects any session (internal or external) without
/// attaching to it.
/// What: runs `TmuxDriver::monitor_session` for the last 100 pane lines and
/// returns the [`SessionSnapshot`]. A missing session or absent tmux is `404`.
/// Test: `tmux_snapshot_unknown_session_is_404` (covers the no-tmux path).
#[utoipa::path(
    get,
    path = "/tmux/sessions/{name}/snapshot",
    tag = "tmux",
    params(("name" = String, Path, description = "tmux session name")),
    responses(
        (status = 200, description = "Session snapshot"),
        (status = 404, description = "Session not found or tmux unavailable"),
    )
)]
pub async fn tmux_snapshot(
    State(_state): State<Arc<DaemonState>>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let driver = TmuxDriver::discover().map_err(|_| StatusCode::NOT_FOUND)?;
    match driver.monitor_session(&name, 100) {
        Ok(snapshot) => Ok(Json(serde_json::json!({ "snapshot": snapshot }))),
        Err(e) => {
            tracing::warn!("tmux snapshot for {name} failed: {e}");
            Err(StatusCode::NOT_FOUND)
        }
    }
}

/// JSON body for `POST /tmux/adopt`.
///
/// Why: adopting an external session needs only its name.
/// What: the tmux session name to bring under oversight.
/// Test: `adopt_tmux_session_handles_missing`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct AdoptRequest {
    /// tmux session name to adopt.
    pub session: String,
}

/// `POST /tmux/adopt` — register an external tmux session for oversight.
///
/// Why: trusty-mpm should watch sessions it did not create; adoption is the
/// explicit, non-destructive opt-in for that.
/// What: runs `TmuxDriver::adopt_session` (which captures the session's shape
/// without modifying it) and returns the [`AdoptedSession`]. A missing session
/// or absent tmux is `404`.
/// Test: `adopt_tmux_session_handles_missing`.
#[utoipa::path(
    post,
    path = "/tmux/adopt",
    tag = "tmux",
    request_body = AdoptRequest,
    responses(
        (status = 200, description = "Session adopted; returns its captured state"),
        (status = 404, description = "Session not found or tmux unavailable"),
    )
)]
pub async fn adopt_tmux_session(
    State(_state): State<Arc<DaemonState>>,
    Json(body): Json<AdoptRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let driver = TmuxDriver::discover().map_err(|_| StatusCode::NOT_FOUND)?;
    match driver.adopt_session(&body.session) {
        Ok(adopted) => Ok(Json(serde_json::json!({ "adopted": adopted }))),
        Err(e) => {
            tracing::warn!("tmux adopt {} failed: {e}", body.session);
            Err(StatusCode::NOT_FOUND)
        }
    }
}

// ---- Claude Code configuration analyzer ---------------------------------

/// Query parameters for `GET /claude-config`.
///
/// Why: the analyzer inspects the config for a specific project directory.
/// What: the absolute project path to analyze.
/// Test: `get_claude_config_returns_recommendations`.
#[derive(serde::Deserialize)]
pub struct ClaudeConfigQuery {
    /// Project directory whose Claude Code config to analyze.
    pub project: PathBuf,
}

/// `GET /claude-config?project=<path>` — analyze Claude Code config.
///
/// Why: trusty-mpm can recommend config changes (hooks, permission scoping,
/// agent deployment) for a project's Claude Code setup.
/// What: resolves the user- and project-level config paths, reads and merges
/// them, and returns `{ config, recommendations }`.
/// Test: `get_claude_config_returns_recommendations`.
#[utoipa::path(
    get,
    path = "/claude-config",
    tag = "claude-config",
    params(("project" = String, Query, description = "Project directory")),
    responses((status = 200, description = "Analyzed config plus recommendations"))
)]
pub async fn get_claude_config(
    State(_state): State<Arc<DaemonState>>,
    Query(query): Query<ClaudeConfigQuery>,
) -> Json<serde_json::Value> {
    use trusty_mpm_core::claude_config::ClaudeConfigReader;
    let paths = ClaudeConfigReader::paths_for_project(&query.project);
    let config = crate::claude_config::ClaudeConfigAnalyzer::read_config(&paths);
    let recommendations = crate::claude_config::ClaudeConfigAnalyzer::analyze(&config);
    Json(serde_json::json!({
        "config": config,
        "recommendations": recommendations,
    }))
}

/// JSON body for `POST /claude-config/apply`.
///
/// Why: applying a recommendation needs the project path and the rec id.
/// What: the project directory and the recommendation id to apply.
/// Test: `apply_claude_config_unknown_rec_is_404`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct ApplyConfigRequest {
    /// Project directory the recommendation applies to.
    #[schema(value_type = String)]
    pub project: PathBuf,
    /// Id of the recommendation to apply.
    pub recommendation_id: String,
}

/// `POST /claude-config/apply` — apply a Claude Code config recommendation.
///
/// Why: lets an operator act on a recommendation without hand-editing JSON.
/// What: re-analyzes the project, finds the recommendation by id, and applies
/// it via `ClaudeConfigAnalyzer::apply_recommendation`, which checkpoints the
/// config first. Returns `{ applied: true, checkpoint_id }` so the caller can
/// undo. An unknown id is `404`.
/// Test: `apply_claude_config_unknown_rec_is_404`.
#[utoipa::path(
    post,
    path = "/claude-config/apply",
    tag = "claude-config",
    request_body = ApplyConfigRequest,
    responses(
        (status = 200, description = "Recommendation applied; returns checkpoint id"),
        (status = 404, description = "No recommendation with that id"),
        (status = 500, description = "Applying the recommendation failed"),
    )
)]
pub async fn apply_claude_config(
    State(_state): State<Arc<DaemonState>>,
    Json(body): Json<ApplyConfigRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use trusty_mpm_core::claude_config::ClaudeConfigReader;
    let paths = ClaudeConfigReader::paths_for_project(&body.project);
    let config = crate::claude_config::ClaudeConfigAnalyzer::read_config(&paths);
    let recommendations = crate::claude_config::ClaudeConfigAnalyzer::analyze(&config);
    let rec = recommendations
        .iter()
        .find(|r| r.id == body.recommendation_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let checkpoint_id = crate::claude_config::ClaudeConfigAnalyzer::apply_recommendation(
        rec,
        &paths,
        &body.project,
    )
    .map_err(|e| {
        tracing::warn!("applying recommendation {} failed: {e}", rec.id);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(serde_json::json!({
        "applied": true,
        "recommendation_id": body.recommendation_id,
        "checkpoint_id": checkpoint_id,
    })))
}

// ---- checkpoints & deployment profiles ----------------------------------

/// Query parameters for the checkpoint list / delete endpoints.
///
/// Why: checkpoints are project-scoped; the project path identifies which
/// `.trusty-mpm/checkpoints` directory to operate on.
/// What: the project directory.
/// Test: `list_checkpoints_returns_array`.
#[derive(serde::Deserialize)]
pub struct CheckpointQuery {
    /// Project directory whose checkpoints to operate on.
    pub project: PathBuf,
}

/// `GET /claude-config/checkpoints?project=<path>` — list config checkpoints.
///
/// Why: the dashboard offers a restore picker; this feeds it.
/// What: returns `{ checkpoints: [ConfigCheckpoint, ...] }`, newest first.
/// Test: `list_checkpoints_returns_array`.
#[utoipa::path(
    get,
    path = "/claude-config/checkpoints",
    tag = "claude-config",
    params(("project" = String, Query, description = "Project directory")),
    responses((status = 200, description = "Config checkpoints, newest first"))
)]
pub async fn list_checkpoints(
    State(_state): State<Arc<DaemonState>>,
    Query(query): Query<CheckpointQuery>,
) -> Json<serde_json::Value> {
    let checkpoints = crate::claude_config::ConfigCheckpointer::list(&query.project)
        .unwrap_or_else(|e| {
            tracing::warn!("listing checkpoints failed: {e}");
            Vec::new()
        });
    Json(serde_json::json!({ "checkpoints": checkpoints }))
}

/// JSON body for `POST /claude-config/checkpoints`.
///
/// Why: creating a checkpoint needs the project and an optional human label.
/// What: the project directory and an optional label.
/// Test: `create_checkpoint_returns_id`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct CreateCheckpointRequest {
    /// Project directory to checkpoint.
    #[schema(value_type = String)]
    pub project: PathBuf,
    /// Optional human-readable label for the checkpoint.
    #[serde(default)]
    pub label: Option<String>,
}

/// `POST /claude-config/checkpoints` — create a config checkpoint.
///
/// Why: lets the operator take a manual backup before a risky change.
/// What: snapshots the project's config and returns `{ id }`.
/// Test: `create_checkpoint_returns_id`.
#[utoipa::path(
    post,
    path = "/claude-config/checkpoints",
    tag = "claude-config",
    request_body = CreateCheckpointRequest,
    responses(
        (status = 200, description = "Checkpoint created; returns its id"),
        (status = 500, description = "Creating the checkpoint failed"),
    )
)]
pub async fn create_checkpoint(
    State(_state): State<Arc<DaemonState>>,
    Json(body): Json<CreateCheckpointRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use trusty_mpm_core::claude_config::ClaudeConfigReader;
    let paths = ClaudeConfigReader::paths_for_project(&body.project);
    let id = crate::claude_config::ConfigCheckpointer::create(
        &paths,
        &body.project,
        body.label.as_deref(),
    )
    .map_err(|e| {
        tracing::warn!("creating checkpoint failed: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(serde_json::json!({ "id": id })))
}

/// JSON body for `POST /claude-config/restore`.
///
/// Why: restoring needs the project and the checkpoint id to revert to.
/// What: the project directory and the checkpoint id.
/// Test: `restore_unknown_checkpoint_is_500`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct RestoreRequest {
    /// Project directory whose config to restore.
    #[schema(value_type = String)]
    pub project: PathBuf,
    /// Id of the checkpoint to restore.
    pub checkpoint_id: String,
}

/// `POST /claude-config/restore` — restore config from a checkpoint.
///
/// Why: the undo half of the safety model.
/// What: rewrites the project's config files to the checkpoint's state. A
/// missing or malformed checkpoint surfaces as `500`.
/// Test: `restore_unknown_checkpoint_is_500`.
#[utoipa::path(
    post,
    path = "/claude-config/restore",
    tag = "claude-config",
    request_body = RestoreRequest,
    responses(
        (status = 200, description = "Config restored from the checkpoint"),
        (status = 500, description = "Checkpoint missing or restore failed"),
    )
)]
pub async fn restore_checkpoint(
    State(_state): State<Arc<DaemonState>>,
    Json(body): Json<RestoreRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    crate::claude_config::ConfigCheckpointer::restore(&body.project, &body.checkpoint_id).map_err(
        |e| {
            tracing::warn!("restoring checkpoint {} failed: {e}", body.checkpoint_id);
            StatusCode::INTERNAL_SERVER_ERROR
        },
    )?;
    Ok(Json(serde_json::json!({
        "restored": true,
        "checkpoint_id": body.checkpoint_id,
    })))
}

/// `DELETE /claude-config/checkpoints/{id}?project=<path>` — delete a checkpoint.
///
/// Why: checkpoints accumulate; the operator prunes them here.
/// What: removes the checkpoint file. A missing checkpoint surfaces as `404`.
/// Test: `delete_unknown_checkpoint_is_404`.
#[utoipa::path(
    delete,
    path = "/claude-config/checkpoints/{id}",
    tag = "claude-config",
    params(
        ("id" = String, Path, description = "Checkpoint id"),
        ("project" = String, Query, description = "Project directory"),
    ),
    responses(
        (status = 200, description = "Checkpoint deleted"),
        (status = 404, description = "No checkpoint with that id"),
    )
)]
pub async fn delete_checkpoint(
    State(_state): State<Arc<DaemonState>>,
    Path(id): Path<String>,
    Query(query): Query<CheckpointQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    crate::claude_config::ConfigCheckpointer::delete(&query.project, &id).map_err(|e| {
        tracing::warn!("deleting checkpoint {id} failed: {e}");
        StatusCode::NOT_FOUND
    })?;
    Ok(Json(serde_json::json!({ "deleted": id })))
}

/// `GET /claude-config/profiles` — list the built-in deployment profiles.
///
/// Why: the dashboard shows the available configuration presets.
/// What: returns `{ profiles: [DeploymentProfile, ...] }`.
/// Test: `list_profiles_returns_builtins`.
#[utoipa::path(
    get,
    path = "/claude-config/profiles",
    tag = "claude-config",
    responses((status = 200, description = "Built-in deployment profiles"))
)]
pub async fn list_profiles(State(_state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let profiles = crate::claude_config::ProfileDeployer::builtin_profiles();
    Json(serde_json::json!({ "profiles": profiles }))
}

/// JSON body for `POST /claude-config/deploy`.
///
/// Why: deploying a profile needs the project, the profile name, and an
/// optional target override.
/// What: the project directory, the profile name, and an optional deploy
/// target (`user`, `project`, `both`) overriding the profile's default.
/// Test: `deploy_profile_returns_checkpoint_id`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct DeployProfileRequest {
    /// Project directory to deploy the profile onto.
    #[schema(value_type = String)]
    pub project: PathBuf,
    /// Name of the built-in profile to deploy.
    pub profile_name: String,
    /// Optional deploy-target override (`user`, `project`, `both`).
    #[serde(default)]
    pub target: Option<trusty_mpm_core::claude_config::DeployTarget>,
}

/// `POST /claude-config/deploy` — deploy a built-in profile onto a project.
///
/// Why: lets the operator apply a configuration preset in one click; the deploy
/// checkpoints the config first so it is reversible.
/// What: looks up the named built-in profile (applying an optional `target`
/// override), deploys it, and returns `{ checkpoint_id }`. An unknown profile
/// name is `404`.
/// Test: `deploy_profile_returns_checkpoint_id`, `deploy_unknown_profile_is_404`.
#[utoipa::path(
    post,
    path = "/claude-config/deploy",
    tag = "claude-config",
    request_body = DeployProfileRequest,
    responses(
        (status = 200, description = "Profile deployed; returns checkpoint id"),
        (status = 404, description = "No built-in profile with that name"),
        (status = 500, description = "Deploying the profile failed"),
    )
)]
pub async fn deploy_profile(
    State(_state): State<Arc<DaemonState>>,
    Json(body): Json<DeployProfileRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use trusty_mpm_core::claude_config::ClaudeConfigReader;
    let mut profile = crate::claude_config::ProfileDeployer::builtin_profiles()
        .into_iter()
        .find(|p| p.name == body.profile_name)
        .ok_or(StatusCode::NOT_FOUND)?;
    if let Some(target) = body.target {
        profile.target = target;
    }
    let paths = ClaudeConfigReader::paths_for_project(&body.project);
    let checkpoint_id =
        crate::claude_config::ProfileDeployer::deploy(&profile, &paths, &body.project).map_err(
            |e| {
                tracing::warn!("deploying profile {} failed: {e}", body.profile_name);
                StatusCode::INTERNAL_SERVER_ERROR
            },
        )?;
    Ok(Json(serde_json::json!({
        "deployed": body.profile_name,
        "checkpoint_id": checkpoint_id,
    })))
}

/// JSON body for `POST /claude-config/restart`.
///
/// Why: restarting Claude Code happens inside a named tmux session.
/// What: the tmux session in which to restart `claude`.
/// Test: `restart_claude_code_handles_missing_tmux`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct RestartRequest {
    /// tmux session in which to restart Claude Code.
    pub tmux_session: String,
}

/// `POST /claude-config/restart` — restart Claude Code in a tmux session.
///
/// Why: after applying config changes the operator wants a clean Claude Code
/// process; this sends Ctrl-C then `claude` into the session's pane.
/// What: calls `ClaudeCodeRestarter::restart_in_session`. tmux being absent
/// surfaces as `500`.
/// Test: `restart_claude_code_handles_missing_tmux`.
#[utoipa::path(
    post,
    path = "/claude-config/restart",
    tag = "claude-config",
    request_body = RestartRequest,
    responses(
        (status = 200, description = "Restart command sent"),
        (status = 500, description = "tmux unavailable or restart failed"),
    )
)]
pub async fn restart_claude_code(
    State(_state): State<Arc<DaemonState>>,
    Json(body): Json<RestartRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    crate::claude_config::ClaudeCodeRestarter::restart_in_session(&body.tmux_session).map_err(
        |e| {
            tracing::warn!("restart in {} failed: {e}", body.tmux_session);
            StatusCode::INTERNAL_SERVER_ERROR
        },
    )?;
    Ok(Json(serde_json::json!({ "restarted": body.tmux_session })))
}

// ---- bot pairing --------------------------------------------------------

/// `POST /pair/request` — generate a one-time Telegram-bot pairing code.
///
/// Why: pairing the Telegram bot to this daemon needs an out-of-band shared
/// secret; `tm pair` calls this on the local daemon to obtain a short code the
/// operator then types into the bot.
/// What: generates a six-character code (stored with a five-minute TTL) and
/// returns `{ "code", "expires_in_seconds" }`.
/// Test: `pair_request_returns_code`.
#[utoipa::path(
    post,
    path = "/pair/request",
    tag = "config",
    responses((status = 200, description = "A one-time pairing code and its TTL"))
)]
pub async fn pair_request(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let code = state.generate_pair_code();
    Json(serde_json::json!({
        "code": code,
        "expires_in_seconds": crate::state::PAIR_CODE_TTL.as_secs(),
    }))
}

/// JSON body for `POST /pair/confirm`.
///
/// Why: confirming a pairing needs the operator's code and the Telegram chat id
/// to bind.
/// What: the six-character code and the chat id.
/// Test: `pair_confirm_validates_code`.
#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct PairConfirmRequest {
    /// The one-time pairing code issued by `POST /pair/request`.
    pub code: String,
    /// The Telegram chat id to pair with this daemon.
    pub chat_id: i64,
}

/// `POST /pair/confirm` — confirm a pairing code and register the chat.
///
/// Why: the Telegram bot's `/pair <code>` flow validates the operator's code so
/// push alerts have an authenticated destination.
/// What: validates `code` against the outstanding code within its TTL; on
/// success stores `chat_id` and returns `{ "success": true, "chat_id" }`,
/// otherwise `{ "success": false, "error": "invalid or expired code" }`.
/// Test: `pair_confirm_validates_code`, `pair_confirm_rejects_bad_code`.
#[utoipa::path(
    post,
    path = "/pair/confirm",
    tag = "config",
    request_body = PairConfirmRequest,
    responses((status = 200, description = "Pairing result (success flag and chat id or error)"))
)]
pub async fn pair_confirm(
    State(state): State<Arc<DaemonState>>,
    Json(body): Json<PairConfirmRequest>,
) -> Json<serde_json::Value> {
    if state.confirm_pair_code(&body.code, body.chat_id) {
        Json(serde_json::json!({ "success": true, "chat_id": body.chat_id }))
    } else {
        Json(serde_json::json!({
            "success": false,
            "error": "invalid or expired code",
        }))
    }
}

/// `GET /pair/status` — report whether a Telegram chat is paired.
///
/// Why: the bot's `/start` command branches on whether the daemon is already
/// paired so it shows either a welcome-and-pair prompt or a ready message.
/// What: returns `{ "paired": <bool>, "chat_id": <i64 or null> }`.
/// Test: `pair_status_reports_unpaired`.
#[utoipa::path(
    get,
    path = "/pair/status",
    tag = "config",
    responses((status = 200, description = "Pairing status (paired flag and chat id)"))
)]
pub async fn pair_status(State(state): State<Arc<DaemonState>>) -> Json<serde_json::Value> {
    let chat_id = state.paired_chat_id();
    Json(serde_json::json!({
        "paired": chat_id.is_some(),
        "chat_id": chat_id,
    }))
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
    async fn pause_then_resume_round_trips() {
        // Pausing flips a session to `Paused`; resuming flips it back to
        // `Active` and clears the pause metadata.
        let (state, id) = state_with_session();
        let Json(body) = pause_session(
            State(Arc::clone(&state)),
            Path(id.0.to_string()),
            Json(PauseRequest {
                summary: Some("mid-task".into()),
            }),
        )
        .await
        .expect("pause succeeds");
        assert_eq!(body["paused"], true);
        assert_eq!(body["summary"], "mid-task");

        let paused = state.session(id).expect("session exists");
        assert_eq!(paused.status, SessionStatus::Paused);
        assert_eq!(paused.pause_summary.as_deref(), Some("mid-task"));
        assert!(paused.paused_at.is_some());

        let Json(resumed) = resume_session(State(Arc::clone(&state)), Path(id.0.to_string()))
            .await
            .expect("resume succeeds");
        assert_eq!(resumed["resumed"], true);

        let active = state.session(id).expect("session exists");
        assert_eq!(active.status, SessionStatus::Active);
        assert_eq!(active.paused_at, None);
        assert_eq!(active.pause_summary, None);
    }

    #[tokio::test]
    async fn pause_unknown_session_is_404() {
        let state = DaemonState::shared();
        let err = pause_session(
            State(state),
            Path(SessionId::new().0.to_string()),
            Json(PauseRequest::default()),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn resume_unpaused_session_is_409() {
        // A session that was never paused cannot be resumed.
        let (state, id) = state_with_session();
        let err = resume_session(State(state), Path(id.0.to_string()))
            .await
            .unwrap_err();
        assert_eq!(err, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn send_command_returns_output_shape() {
        // The command endpoint always returns `{ sent, output }`; tmux errors
        // are swallowed so the output may simply be empty.
        let (state, id) = state_with_session();
        let Json(body) = send_command(
            State(state),
            Path(id.0.to_string()),
            Query(CommandQuery::default()),
            Json(CommandRequest {
                command: "help".into(),
            }),
        )
        .await
        .expect("command sent");
        assert_eq!(body["sent"], true);
        assert!(body["output"].is_string());
    }

    #[tokio::test]
    async fn command_to_stopped_session_is_409() {
        let state = DaemonState::shared();
        let id = SessionId::new();
        let mut session = Session::new(id, "/tmp/p", ControlModel::Tmux);
        session.status = SessionStatus::Stopped;
        state.register_session(session);

        let err = send_command(
            State(state),
            Path(id.0.to_string()),
            Query(CommandQuery::default()),
            Json(CommandRequest {
                command: "help".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn get_output_returns_output_shape() {
        let (state, id) = state_with_session();
        let Json(body) = get_output(
            State(state),
            Path(id.0.to_string()),
            Query(OutputQuery {
                lines: Some(25),
                compress: None,
            }),
        )
        .await
        .expect("output captured");
        assert!(body["output"].is_string());
        assert_eq!(body["lines"], 25);
    }

    #[tokio::test]
    async fn output_unknown_session_is_404() {
        let state = DaemonState::shared();
        let err = get_output(
            State(state),
            Path(SessionId::new().0.to_string()),
            Query(OutputQuery::default()),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pause_resolves_session_by_friendly_name() {
        // The pause endpoint accepts a friendly tmux name, not just a UUID.
        let (state, id) = state_with_session();
        let name = state.session(id).expect("session").tmux_name;
        let Json(body) = pause_session(
            State(Arc::clone(&state)),
            Path(name),
            Json(PauseRequest::default()),
        )
        .await
        .expect("pause by name succeeds");
        assert_eq!(body["paused"], true);
    }

    #[tokio::test]
    async fn get_optimizer_returns_default() {
        let state = DaemonState::shared();
        let Json(body) = get_optimizer(State(state)).await;
        assert_eq!(body["optimizer"]["default_level"], "trim");
        assert_eq!(body["optimizer"]["suppress_redundant_reads"], true);
    }

    #[test]
    fn send_command_compress_query_defaults_off() {
        // A `CommandQuery` with no `compress` field deserializes to `None`, so
        // omitting `?compress=` defaults to no compression.
        let query: CommandQuery = serde_json::from_str("{}").expect("empty query deserializes");
        assert_eq!(query.compress, None);
    }

    #[test]
    fn output_query_defaults() {
        // An `OutputQuery` with no fields set has neither a line count nor a
        // compression level.
        let query: OutputQuery = serde_json::from_str("{}").expect("empty query deserializes");
        assert_eq!(query.lines, None);
        assert_eq!(query.compress, None);
    }

    #[test]
    fn compress_level_roundtrips_serde() {
        // `CompressionLevel::Summarise` serializes to the lowercase wire name
        // `"summarise"` and deserializes back to the same variant.
        let json = serde_json::to_string(&CompressionLevel::Summarise).expect("serialize");
        assert_eq!(json, "\"summarise\"");
        let parsed: CompressionLevel = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, CompressionLevel::Summarise);
    }

    #[test]
    fn compress_level_label_matches_serde() {
        // The lowercase label helper agrees with serde's wire representation.
        assert_eq!(compression_level_label(CompressionLevel::Off), "off");
        assert_eq!(compression_level_label(CompressionLevel::Trim), "trim");
        assert_eq!(
            compression_level_label(CompressionLevel::Summarise),
            "summarise"
        );
        assert_eq!(
            compression_level_label(CompressionLevel::Caveman),
            "caveman"
        );
    }

    #[test]
    fn apply_compression_off_is_passthrough() {
        // With no level, the text is returned unchanged and there is no label.
        let result = apply_compression(None, "raw pane text");
        assert_eq!(result.text, "raw pane text");
        assert_eq!(result.level_label, None);
    }

    #[test]
    fn apply_compression_summarise() {
        // With a level set, the label is recorded and stats reflect the input.
        let raw = "x".repeat(100);
        let result = apply_compression(Some(CompressionLevel::Summarise), &raw);
        assert_eq!(result.level_label.as_deref(), Some("summarise"));
        assert_eq!(result.stats.original_bytes, 100);
    }

    #[tokio::test]
    async fn list_tmux_sessions_returns_array() {
        // `GET /tmux/sessions` always returns a well-formed `sessions` array
        // (empty when tmux is unavailable in CI).
        let state = DaemonState::shared();
        let Json(body) = list_tmux_sessions(State(state)).await;
        assert!(body["sessions"].is_array());
    }

    #[tokio::test]
    async fn adopt_tmux_session_handles_missing() {
        // Adopting a session that does not exist (or with tmux absent) is 404.
        let state = DaemonState::shared();
        let result = adopt_tmux_session(
            State(state),
            Json(AdoptRequest {
                session: "trusty-mpm-no-such-session-xyz".into(),
            }),
        )
        .await;
        assert_eq!(result.unwrap_err(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn tmux_snapshot_unknown_session_is_404() {
        let state = DaemonState::shared();
        let result = tmux_snapshot(State(state), Path("no-such-session-xyz".into())).await;
        assert_eq!(result.unwrap_err(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_claude_config_returns_shape() {
        // `GET /claude-config` returns a `config` object and a
        // `recommendations` array. The exact recommendations depend on the
        // host's real `~/.claude` (user-level settings are merged in), so the
        // test asserts only the response *shape*, not specific entries.
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::shared();
        let Json(body) = get_claude_config(
            State(state),
            Query(ClaudeConfigQuery {
                project: dir.path().to_path_buf(),
            }),
        )
        .await;
        assert!(body["config"].is_object());
        assert!(body["recommendations"].is_array());
    }

    #[tokio::test]
    async fn list_checkpoints_returns_array() {
        // `GET /claude-config/checkpoints` returns a well-formed array even for
        // a project with no checkpoints yet.
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::shared();
        let Json(body) = list_checkpoints(
            State(state),
            Query(CheckpointQuery {
                project: dir.path().to_path_buf(),
            }),
        )
        .await;
        assert!(body["checkpoints"].is_array());
    }

    #[tokio::test]
    async fn create_checkpoint_returns_id() {
        // `POST /claude-config/checkpoints` returns an `id` and the checkpoint
        // is then visible via the list endpoint.
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::shared();
        let Json(body) = create_checkpoint(
            State(Arc::clone(&state)),
            Json(CreateCheckpointRequest {
                project: dir.path().to_path_buf(),
                label: Some("manual".into()),
            }),
        )
        .await
        .expect("create succeeds");
        assert!(body["id"].as_str().is_some());

        let Json(listed) = list_checkpoints(
            State(state),
            Query(CheckpointQuery {
                project: dir.path().to_path_buf(),
            }),
        )
        .await;
        assert_eq!(listed["checkpoints"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restore_unknown_checkpoint_is_500() {
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::shared();
        let err = restore_checkpoint(
            State(state),
            Json(RestoreRequest {
                project: dir.path().to_path_buf(),
                checkpoint_id: "no-such-checkpoint".into(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn delete_unknown_checkpoint_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::shared();
        let err = delete_checkpoint(
            State(state),
            Path("no-such-checkpoint".into()),
            Query(CheckpointQuery {
                project: dir.path().to_path_buf(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn list_profiles_returns_builtins() {
        // `GET /claude-config/profiles` lists the three built-in profiles.
        let state = DaemonState::shared();
        let Json(body) = list_profiles(State(state)).await;
        let profiles = body["profiles"].as_array().expect("profiles array");
        assert_eq!(profiles.len(), 3);
    }

    #[tokio::test]
    async fn deploy_profile_returns_checkpoint_id() {
        // `POST /claude-config/deploy` deploys a built-in profile and returns a
        // checkpoint id for undo.
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::shared();
        let Json(body) = deploy_profile(
            State(state),
            Json(DeployProfileRequest {
                project: dir.path().to_path_buf(),
                profile_name: "minimal".into(),
                target: None,
            }),
        )
        .await
        .expect("deploy succeeds");
        assert_eq!(body["deployed"], "minimal");
        assert!(body["checkpoint_id"].as_str().is_some());
    }

    #[tokio::test]
    async fn deploy_unknown_profile_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::shared();
        let err = deploy_profile(
            State(state),
            Json(DeployProfileRequest {
                project: dir.path().to_path_buf(),
                profile_name: "no-such-profile".into(),
                target: None,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pair_request_returns_code() {
        // `POST /pair/request` returns a six-character code and a TTL.
        let state = DaemonState::shared();
        let Json(body) = pair_request(State(state)).await;
        let code = body["code"].as_str().expect("code is a string");
        assert_eq!(code.len(), 6);
        assert_eq!(body["expires_in_seconds"], 300);
    }

    #[tokio::test]
    async fn pair_confirm_validates_code() {
        // A code from `/pair/request` confirms successfully, and `/pair/status`
        // then reports the daemon as paired with that chat.
        let state = DaemonState::shared();
        let Json(req) = pair_request(State(Arc::clone(&state))).await;
        let code = req["code"].as_str().unwrap().to_string();
        let Json(confirm) = pair_confirm(
            State(Arc::clone(&state)),
            Json(PairConfirmRequest { code, chat_id: 777 }),
        )
        .await;
        assert_eq!(confirm["success"], true);
        assert_eq!(confirm["chat_id"], 777);

        let Json(status) = pair_status(State(state)).await;
        assert_eq!(status["paired"], true);
        assert_eq!(status["chat_id"], 777);
    }

    #[tokio::test]
    async fn pair_confirm_rejects_bad_code() {
        // A code that was never issued must not pair the daemon.
        let state = DaemonState::shared();
        let _ = pair_request(State(Arc::clone(&state))).await;
        let Json(confirm) = pair_confirm(
            State(Arc::clone(&state)),
            Json(PairConfirmRequest {
                code: "ZZZZZZ".into(),
                chat_id: 777,
            }),
        )
        .await;
        assert_eq!(confirm["success"], false);
        assert!(confirm["error"].as_str().unwrap().contains("invalid"));

        let Json(status) = pair_status(State(state)).await;
        assert_eq!(status["paired"], false);
        assert!(status["chat_id"].is_null());
    }

    #[tokio::test]
    async fn pair_status_reports_unpaired() {
        let state = DaemonState::shared();
        let Json(status) = pair_status(State(state)).await;
        assert_eq!(status["paired"], false);
    }

    #[tokio::test]
    async fn apply_claude_config_unknown_rec_is_404() {
        let dir = tempfile::tempdir().unwrap();
        let state = DaemonState::shared();
        let result = apply_claude_config(
            State(state),
            Json(ApplyConfigRequest {
                project: dir.path().to_path_buf(),
                recommendation_id: "no-such-recommendation".into(),
            }),
        )
        .await;
        assert_eq!(result.unwrap_err(), StatusCode::NOT_FOUND);
    }
}
