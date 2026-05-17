//! Tauri IPC commands for the trusty-mpm desktop shell.
//!
//! Why: The desktop app must never embed business logic — all fleet state
//! lives in the daemon. These commands are a 1:1 proxy layer so the Svelte
//! frontend can use the same `invoke(...)` surface it uses in web mode.
//! What: Five commands (`get_daemon_url`, `check_health`, `list_sessions`,
//! `pause_session`, `resume_session`) that each forward to the daemon REST
//! API and surface errors as `String` for the frontend.
//! Test: Run the daemon on `127.0.0.1:7880`, invoke each command, and assert
//! the returned JSON matches the corresponding `curl` against the daemon.

use serde_json::Value;
use tauri::State;

use crate::state::GuiState;

/// Return the configured daemon base URL.
///
/// Why: The frontend's transport layer shows the active daemon URL in the
/// settings panel; in Tauri mode it must ask the Rust side rather than read
/// `localStorage`.
/// What: Echoes `GuiState::daemon_url`.
/// Test: Invoke with `TRUSTY_MPM_URL` unset → returns the default URL.
#[tauri::command]
pub fn get_daemon_url(state: State<'_, GuiState>) -> String {
    state.daemon_url.clone()
}

/// `GET /health` — check daemon liveness.
///
/// Why: The header health dot polls this to tell the user whether the daemon
/// is reachable before any other command will succeed.
/// What: Issues a GET to `/health`; returns `true` on a 2xx response, `false`
/// on any transport error or non-success status.
/// Test: With the daemon down, assert `false`; with it up, assert `true`.
#[tauri::command]
pub async fn check_health(state: State<'_, GuiState>) -> Result<bool, String> {
    let url = format!("{}/health", state.daemon_url);
    match state.client.get(&url).send().await {
        Ok(resp) => Ok(resp.status().is_success()),
        Err(_) => Ok(false),
    }
}

/// `GET /sessions` — list all registered sessions.
///
/// Why: Drives the left sidebar `SessionList`; proxied so web and desktop
/// share one data path.
/// What: Forwards to `/sessions` and returns the raw JSON body untouched.
/// Test: Register a session via the daemon, invoke this, assert the session
/// id appears in the returned array.
#[tauri::command]
pub async fn list_sessions(state: State<'_, GuiState>) -> Result<Value, String> {
    let url = format!("{}/sessions", state.daemon_url);
    let resp = state
        .client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("list_sessions request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("list_sessions: HTTP {}", resp.status()));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| format!("list_sessions parse failed: {e}"))
}

/// `POST /sessions/{id}/pause` — pause a running session.
///
/// Why: The `SessionList` pause button must reach the daemon's session
/// supervisor; the shell only relays the call.
/// What: POSTs to `/sessions/{id}/pause` and returns the JSON ack.
/// Test: Pause a running session, then `GET /sessions` and assert its status
/// is `paused`.
#[tauri::command]
pub async fn pause_session(id: String, state: State<'_, GuiState>) -> Result<Value, String> {
    post_session_action(&state, &id, "pause").await
}

/// `POST /sessions/{id}/resume` — resume a paused session.
///
/// Why: Mirror of `pause_session` for the resume button.
/// What: POSTs to `/sessions/{id}/resume` and returns the JSON ack.
/// Test: Resume a paused session and assert its status returns to `running`.
#[tauri::command]
pub async fn resume_session(id: String, state: State<'_, GuiState>) -> Result<Value, String> {
    post_session_action(&state, &id, "resume").await
}

/// `DELETE /sessions/{id}` — stop and remove a session.
///
/// Why: The `SessionList` stop button needs a way to terminate a session from
/// the desktop shell; the shell relays the DELETE to the daemon.
/// What: Sends `DELETE /sessions/{id}` and returns the JSON ack or `Null`.
/// Test: Stop a registered session, then `GET /sessions` and assert the id no
/// longer appears.
#[tauri::command]
pub async fn stop_session(id: String, state: State<'_, GuiState>) -> Result<Value, String> {
    let url = format!("{}/sessions/{id}", state.daemon_url);
    let resp = state
        .client
        .delete(&url)
        .send()
        .await
        .map_err(|e| format!("stop_session request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("stop_session: HTTP {}", resp.status()));
    }
    Ok(resp.json::<Value>().await.unwrap_or(Value::Null))
}

/// `GET /breakers` — fetch all circuit-breaker states.
///
/// Why: `SessionDetail` shows per-agent breaker state; the shell proxies this
/// so the desktop and web builds use the same code path.
/// What: Forwards to `/breakers` and returns the raw JSON body.
/// Test: Invoke with a live daemon; assert the returned object contains at
/// least a `breakers` key.
#[tauri::command]
pub async fn get_breakers(state: State<'_, GuiState>) -> Result<Value, String> {
    let url = format!("{}/breakers", state.daemon_url);
    let resp = state
        .client
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("get_breakers request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("get_breakers: HTTP {}", resp.status()));
    }
    resp.json::<Value>()
        .await
        .map_err(|e| format!("get_breakers parse failed: {e}"))
}

/// Shared helper for `POST /sessions/{id}/{action}` calls.
///
/// Why: `pause_session` and `resume_session` differ only by the action
/// segment; one helper keeps the proxy logic in a single place.
/// What: Builds the URL, POSTs an empty body, and parses the JSON response;
/// tolerates an empty body by returning `Value::Null`.
/// Test: Call with `action = "pause"` against a live daemon and assert no
/// error is returned.
async fn post_session_action(state: &GuiState, id: &str, action: &str) -> Result<Value, String> {
    let url = format!("{}/sessions/{id}/{action}", state.daemon_url);
    let resp = state
        .client
        .post(&url)
        .send()
        .await
        .map_err(|e| format!("{action}_session request failed: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{action}_session: HTTP {}", resp.status()));
    }
    Ok(resp.json::<Value>().await.unwrap_or(Value::Null))
}
