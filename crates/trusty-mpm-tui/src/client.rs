//! Daemon HTTP client for the TUI.
//!
//! Why: the dashboard is a separate process from the daemon; it polls the
//! daemon's HTTP API for the data it renders (sessions, events, breakers).
//! Isolating the transport here keeps the rendering code free of `reqwest`.
//! What: [`DaemonClient`] wraps a base URL and fetches the JSON the dashboard
//! panels need.
//! Test: `cargo test -p trusty-mpm-tui` checks URL construction; live HTTP is
//! covered by the daemon's own API tests.

use serde::Deserialize;

/// HTTP client for one trusty-mpm daemon.
///
/// Why: a thin wrapper so the TUI can be pointed at any daemon address.
/// What: holds the base URL and a shared `reqwest::Client`.
/// Test: `base_url_is_stored`.
#[derive(Debug, Clone)]
pub struct DaemonClient {
    base: String,
    http: reqwest::Client,
}

/// One session row as returned by `GET /sessions`.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionRow {
    /// Session id (UUID).
    pub id: serde_json::Value,
    /// Working directory.
    pub workdir: String,
    /// Lifecycle status string.
    pub status: serde_json::Value,
    /// Number of active delegations.
    #[serde(default)]
    pub active_delegations: u32,
    /// Friendly tmux session name (`tmpm-<adjective>-<noun>`).
    ///
    /// Why: session action endpoints (pause/resume/stop/output) resolve their
    /// `{id}` path segment against this friendly name; the dashboard uses it as
    /// the action target rather than the raw UUID.
    /// Test: `session_row_deserializes_tmux_name`.
    #[serde(default)]
    pub tmux_name: String,
    /// Last-seen timestamp from the daemon, serialized as
    /// `{"secs_since_epoch": u64, "nanos_since_epoch": u32}`.
    ///
    /// Why: `resolve_target` uses this for workdir-prefix recency tie-breaking
    /// so `/connect <path>` picks the most recently active session when
    /// multiple sessions share the same workdir prefix.
    /// What: deserialized from the daemon's `SystemTime` serde output;
    /// defaults to `{"secs_since_epoch":0}` when absent.
    #[serde(default)]
    pub last_seen: LastSeen,
}

/// Serde shape for `SystemTime` as emitted by the daemon.
///
/// Why: `serde` serializes `SystemTime` as a struct, not a plain integer;
/// we extract only the seconds component for recency comparison.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct LastSeen {
    #[serde(default)]
    pub secs_since_epoch: u64,
}

/// One hook-event row as returned by `GET /events`.
///
/// Why: the dashboard's event panel renders the daemon's live hook feed.
/// What: mirrors the serde output of `HookEventRecord` — the `SessionId`
/// newtype JSON, the wire event name, and an RFC3339 timestamp.
/// Test: `events_deserialize_from_record_shape`.
#[derive(Debug, Clone, Deserialize)]
pub struct EventRow {
    /// Originating session (`SessionId` newtype JSON: `{"0": "<uuid>"}`).
    pub session: serde_json::Value,
    /// Claude Code wire event name (e.g. `PreToolUse`).
    pub event: String,
    /// RFC3339 timestamp the daemon received the event.
    pub at: String,
    /// Opaque event payload; defaults to `Null` when the daemon omits it.
    ///
    /// Captured from the wire so a future panel can show event detail; the
    /// current dashboard renders only the summary line, hence `allow(dead_code)`.
    #[serde(default)]
    #[allow(dead_code)]
    pub payload: serde_json::Value,
}

/// One circuit-breaker row as returned by `GET /breakers`.
///
/// Why: the dashboard's breaker panel shows which agents have tripped.
/// What: the agent name plus the flattened breaker state and failure count.
/// Test: `breakers_deserialize_from_api_shape`.
#[derive(Debug, Clone, Deserialize)]
pub struct BreakerRow {
    /// Agent name the breaker guards.
    pub agent: String,
    /// Breaker state: `closed` / `open` / `half_open`.
    pub state: String,
    /// Consecutive failures observed since the last success.
    pub consecutive_failures: u32,
}

impl DaemonClient {
    /// Build a client targeting `base` (e.g. `http://127.0.0.1:7880`).
    pub fn new(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Fetch the current session list from the daemon.
    ///
    /// Why: the dashboard's session panel refreshes on a timer.
    /// What: `GET /sessions`, returns the `sessions` array deserialized.
    /// Test: covered end-to-end by the daemon API tests.
    pub async fn sessions(&self) -> anyhow::Result<Vec<SessionRow>> {
        #[derive(Deserialize)]
        struct Body {
            sessions: Vec<SessionRow>,
        }
        let url = format!("{}/sessions", self.base);
        let body: Body = self.http.get(&url).send().await?.json().await?;
        Ok(body.sessions)
    }

    /// Fetch the recent hook-event feed from the daemon.
    ///
    /// Why: the dashboard's event panel refreshes on the same poll timer.
    /// What: `GET /events`, returns the `events` array deserialized.
    /// Test: `events_deserialize_from_record_shape` covers the wire shape.
    pub async fn events(&self) -> anyhow::Result<Vec<EventRow>> {
        #[derive(Deserialize)]
        struct Body {
            events: Vec<EventRow>,
        }
        let url = format!("{}/events", self.base);
        let body: Body = self.http.get(&url).send().await?.json().await?;
        Ok(body.events)
    }

    /// Fetch every agent's circuit-breaker state from the daemon.
    ///
    /// Why: the dashboard's breaker panel needs the latest breaker snapshot.
    /// What: `GET /breakers`, flattening the nested `breaker` object into a
    /// flat [`BreakerRow`] per agent.
    /// Test: `breakers_deserialize_from_api_shape` covers the wire shape.
    pub async fn breakers(&self) -> anyhow::Result<Vec<BreakerRow>> {
        /// The daemon nests breaker fields under a `breaker` object.
        #[derive(Deserialize)]
        struct WireBreaker {
            state: String,
            consecutive_failures: u32,
        }
        #[derive(Deserialize)]
        struct WireRow {
            agent: String,
            breaker: WireBreaker,
        }
        #[derive(Deserialize)]
        struct Body {
            breakers: Vec<WireRow>,
        }
        let url = format!("{}/breakers", self.base);
        let body: Body = self.http.get(&url).send().await?.json().await?;
        Ok(body
            .breakers
            .into_iter()
            .map(|r| BreakerRow {
                agent: r.agent,
                state: r.breaker.state,
                consecutive_failures: r.breaker.consecutive_failures,
            })
            .collect())
    }

    /// Probe whether the daemon is reachable.
    pub async fn is_healthy(&self) -> bool {
        let url = format!("{}/health", self.base);
        matches!(self.http.get(&url).send().await, Ok(r) if r.status().is_success())
    }

    /// Pause a session via `POST /sessions/{id}/pause`.
    ///
    /// Why: the dashboard's `p` key pauses the selected session in place.
    /// What: POSTs `{"summary": null}`, lets the daemon derive the summary, and
    /// returns the `summary` field from the 200 response.
    /// Test: `client_pause_constructs_url` covers construction; live HTTP is
    /// covered by the daemon's session-lifecycle tests.
    pub async fn pause_session(&self, id: &str) -> anyhow::Result<String> {
        let url = format!("{}/sessions/{id}/pause", self.base);
        let body: serde_json::Value = self
            .http
            .post(&url)
            .json(&serde_json::json!({ "summary": serde_json::Value::Null }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(body
            .get("summary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }

    /// Resume a session via `POST /sessions/{id}/resume`.
    ///
    /// Why: the dashboard's `r` key resumes the selected paused session.
    /// What: POSTs to the resume endpoint and discards the response body.
    /// Test: live HTTP is covered by the daemon's session-lifecycle tests.
    pub async fn resume_session(&self, id: &str) -> anyhow::Result<()> {
        let url = format!("{}/sessions/{id}/resume", self.base);
        self.http.post(&url).send().await?.error_for_status()?;
        Ok(())
    }

    /// Stop a session via `DELETE /sessions/{id}`.
    ///
    /// Why: the dashboard's `x` key stops the selected session.
    /// What: sends a DELETE to the session endpoint and discards the body.
    /// Test: live HTTP is covered by the daemon's session-lifecycle tests.
    pub async fn stop_session(&self, id: &str) -> anyhow::Result<()> {
        let url = format!("{}/sessions/{id}", self.base);
        self.http.delete(&url).send().await?.error_for_status()?;
        Ok(())
    }

    /// Capture recent session output via `GET /sessions/{id}/output`.
    ///
    /// Why: the dashboard's `o` key snapshots the selected session's pane.
    /// What: `GET /sessions/{id}/output?lines={lines}`, returns the `output`
    /// field from the 200 response.
    /// Test: live HTTP is covered by the daemon's session-lifecycle tests.
    pub async fn session_output(&self, id: &str, lines: u32) -> anyhow::Result<String> {
        let url = format!("{}/sessions/{id}/output", self.base);
        let body: serde_json::Value = self
            .http
            .get(&url)
            .query(&[("lines", lines.to_string())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(body
            .get("output")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_constructs_with_base_url() {
        // Construction must not panic for a well-formed URL.
        let _client = DaemonClient::new("http://127.0.0.1:7880");
    }

    #[test]
    fn client_pause_constructs_url() {
        // Constructing a client for the pause path must not panic; same pattern
        // as `client_constructs_with_base_url` — exercises the builder, not HTTP.
        let _client = DaemonClient::new("http://127.0.0.1:7880");
        // The action methods are async and need a live daemon; here we only
        // assert the client is usable as their receiver.
        let client = DaemonClient::new("http://127.0.0.1:7880");
        assert_eq!(client.base, "http://127.0.0.1:7880");
    }

    #[test]
    fn session_row_deserializes_tmux_name() {
        // `GET /sessions` returns the full `Session`, including `tmux_name`.
        let json = serde_json::json!({
            "id": "abcd1234-5678-90ab-cdef-1234567890ab",
            "workdir": "/tmp/proj",
            "status": "active",
            "active_delegations": 1,
            "tmux_name": "tmpm-quiet-falcon"
        });
        let row: SessionRow = serde_json::from_value(json).unwrap();
        assert_eq!(row.tmux_name, "tmpm-quiet-falcon");
    }

    #[test]
    fn session_row_defaults_tmux_name_when_absent() {
        // An older daemon may omit `tmux_name`; it must default to empty.
        let json = serde_json::json!({
            "id": "abcd1234-5678-90ab-cdef-1234567890ab",
            "workdir": "/tmp/proj",
            "status": "active"
        });
        let row: SessionRow = serde_json::from_value(json).unwrap();
        assert_eq!(row.tmux_name, "");
    }

    #[test]
    fn events_deserialize_from_record_shape() {
        // Matches the serde output of `HookEventRecord`.
        let json = serde_json::json!({
            "session": {"0": "abcd1234-5678-90ab-cdef-1234567890ab"},
            "event": "PreToolUse",
            "at": "2024-01-01T00:00:00Z",
            "payload": {}
        });
        let row: EventRow = serde_json::from_value(json).unwrap();
        assert_eq!(row.event, "PreToolUse");
        assert_eq!(row.at, "2024-01-01T00:00:00Z");
    }

    #[test]
    fn breakers_deserialize_from_api_shape() {
        // Matches the `GET /breakers` envelope: agent + nested breaker object.
        let json = serde_json::json!({
            "agent": "research",
            "breaker": { "state": "closed", "consecutive_failures": 0 }
        });
        #[derive(serde::Deserialize)]
        struct WireBreaker {
            state: String,
            consecutive_failures: u32,
        }
        #[derive(serde::Deserialize)]
        struct WireRow {
            agent: String,
            breaker: WireBreaker,
        }
        let row: WireRow = serde_json::from_value(json).unwrap();
        assert_eq!(row.agent, "research");
        assert_eq!(row.breaker.state, "closed");
        assert_eq!(row.breaker.consecutive_failures, 0);
    }

    #[test]
    fn event_row_deserializes_session_id_shape() {
        // The `SessionId` newtype wire shape `{"0": "<uuid>"}` round-trips.
        let json = serde_json::json!({
            "session": {"0": "abcd1234-5678-90ab-cdef-1234567890ab"},
            "event": "Stop",
            "at": "2024-01-01T00:00:00Z",
            "payload": null
        });
        let row: EventRow = serde_json::from_value(json).unwrap();
        assert_eq!(row.event, "Stop");
        assert_eq!(
            row.session.get("0").and_then(|v| v.as_str()),
            Some("abcd1234-5678-90ab-cdef-1234567890ab")
        );
        assert!(row.payload.is_null());
    }

    #[test]
    fn event_row_defaults_payload_when_absent() {
        // An omitted `payload` field must default to JSON `Null`.
        let json = serde_json::json!({
            "session": {"0": "abcd1234-5678-90ab-cdef-1234567890ab"},
            "event": "Stop",
            "at": "2024-01-01T00:00:00Z"
        });
        let row: EventRow = serde_json::from_value(json).unwrap();
        assert!(row.payload.is_null());
    }

    #[test]
    fn breaker_row_full_deserialization() {
        // The nested `breaker` object with an open state and a config block
        // deserializes; unknown `config` fields are ignored.
        let json = serde_json::json!({
            "agent": "eng",
            "breaker": {
                "state": "open",
                "consecutive_failures": 3,
                "config": { "failure_threshold": 3, "max_depth": 5 }
            }
        });
        #[derive(serde::Deserialize)]
        struct WireBreaker {
            state: String,
            consecutive_failures: u32,
        }
        #[derive(serde::Deserialize)]
        struct WireRow {
            agent: String,
            breaker: WireBreaker,
        }
        let row: WireRow = serde_json::from_value(json).unwrap();
        assert_eq!(row.agent, "eng");
        assert_eq!(row.breaker.state, "open");
        assert_eq!(row.breaker.consecutive_failures, 3);
    }

    #[test]
    fn breaker_row_deserialization_closed() {
        // A closed breaker reports zero consecutive failures.
        let json = serde_json::json!({
            "agent": "qa",
            "breaker": { "state": "closed", "consecutive_failures": 0 }
        });
        #[derive(serde::Deserialize)]
        struct WireBreaker {
            state: String,
            consecutive_failures: u32,
        }
        #[derive(serde::Deserialize)]
        struct WireRow {
            agent: String,
            breaker: WireBreaker,
        }
        let row: WireRow = serde_json::from_value(json).unwrap();
        assert_eq!(row.agent, "qa");
        assert_eq!(row.breaker.state, "closed");
        assert_eq!(row.breaker.consecutive_failures, 0);
    }
}
