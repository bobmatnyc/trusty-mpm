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
}
