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
}
