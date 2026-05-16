//! Shared daemon state.
//!
//! Why: the HTTP API, the MCP server, the hook relay, and the dashboard feed
//! all read and mutate the same picture of the world — managed sessions, their
//! delegation trees, per-agent circuit breakers, recent hook events, and
//! per-session memory usage. A single `Arc`-shared, lock-guarded state keeps
//! them consistent and is the daemon's composition root for dependency
//! injection into request handlers.
//! What: [`DaemonState`] holds `DashMap`s keyed by `SessionId`/agent name plus
//! a bounded ring buffer of recent [`HookEventRecord`]s; methods provide the
//! typed mutations the rest of the daemon needs.
//! Test: `cargo test -p trusty-mpm-daemon` exercises registration, the hook
//! ring-buffer bound, and memory-pressure classification.

use std::collections::VecDeque;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;
use trusty_mpm_core::agent::Delegation;
use trusty_mpm_core::circuit::{CircuitBreaker, CircuitConfig};
use trusty_mpm_core::hook::HookEventRecord;
use trusty_mpm_core::memory::{MemoryConfig, MemoryPressure, MemoryUsage};
use trusty_mpm_core::session::{Session, SessionId};

/// How many recent hook events the daemon retains for the dashboard feed.
///
/// Why: the live event feed needs scrollback, but an unbounded log would leak
/// memory in a long-lived daemon; a ring buffer caps it.
pub const HOOK_HISTORY_LIMIT: usize = 1024;

/// The daemon's shared, mutable view of the world.
///
/// Why: shared via `Arc<DaemonState>` into every axum handler and the MCP
/// backend — one source of truth, no global statics.
/// What: concurrent maps for sessions / delegations / breakers / memory, plus
/// a mutex-guarded ring buffer of hook events and the threshold configs.
/// Test: `register_and_list_sessions`, `hook_history_is_bounded`.
#[derive(Debug)]
pub struct DaemonState {
    /// Managed sessions, keyed by id.
    sessions: DashMap<SessionId, Session>,
    /// Active delegations, keyed by delegation id.
    delegations: DashMap<uuid::Uuid, Delegation>,
    /// Circuit breakers, keyed by agent name.
    breakers: DashMap<String, CircuitBreaker>,
    /// Latest token-usage snapshot per session.
    memory: DashMap<SessionId, MemoryUsage>,
    /// Bounded ring buffer of the most recent hook events.
    hook_history: Mutex<VecDeque<HookEventRecord>>,
    /// Memory-protection thresholds (warn / alert / compact).
    pub memory_config: MemoryConfig,
    /// Circuit-breaker tuning applied to newly-seen agents.
    pub circuit_config: CircuitConfig,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonState {
    /// Construct empty state with default thresholds.
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
            delegations: DashMap::new(),
            breakers: DashMap::new(),
            memory: DashMap::new(),
            hook_history: Mutex::new(VecDeque::with_capacity(HOOK_HISTORY_LIMIT)),
            memory_config: MemoryConfig::default(),
            circuit_config: CircuitConfig::default(),
        }
    }

    /// Wrap the state in an `Arc` for sharing across tasks.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    // ---- sessions -------------------------------------------------------

    /// Register (or replace) a managed session.
    pub fn register_session(&self, session: Session) {
        self.sessions.insert(session.id, session);
    }

    /// Remove a session and its associated memory snapshot.
    pub fn remove_session(&self, id: SessionId) -> Option<Session> {
        self.memory.remove(&id);
        self.sessions.remove(&id).map(|(_, s)| s)
    }

    /// Snapshot all managed sessions.
    pub fn list_sessions(&self) -> Vec<Session> {
        self.sessions.iter().map(|e| e.value().clone()).collect()
    }

    /// Look up one session by id.
    pub fn session(&self, id: SessionId) -> Option<Session> {
        self.sessions.get(&id).map(|e| e.value().clone())
    }

    // ---- delegations ----------------------------------------------------

    /// Record a new (or updated) delegation.
    pub fn upsert_delegation(&self, delegation: Delegation) {
        self.delegations.insert(delegation.id.0, delegation);
    }

    /// All delegations belonging to one session.
    pub fn delegations_for(&self, session: SessionId) -> Vec<Delegation> {
        self.delegations
            .iter()
            .filter(|e| e.value().session == session)
            .map(|e| e.value().clone())
            .collect()
    }

    // ---- circuit breakers ----------------------------------------------

    /// Get a snapshot of an agent's circuit breaker, creating a closed one if
    /// the agent has not been seen before.
    pub fn breaker(&self, agent: &str) -> CircuitBreaker {
        self.breakers
            .entry(agent.to_string())
            .or_insert_with(|| CircuitBreaker::new(self.circuit_config))
            .value()
            .clone()
    }

    /// Record a delegation outcome against an agent's breaker.
    ///
    /// Why: the daemon must update breaker state after every delegation so the
    /// next `agent_delegate` call is gated correctly.
    /// What: success/failure drives `record_success` / `record_failure`.
    /// Test: `breaker_tracks_outcomes`.
    pub fn record_outcome(&self, agent: &str, success: bool) {
        let mut entry = self
            .breakers
            .entry(agent.to_string())
            .or_insert_with(|| CircuitBreaker::new(self.circuit_config));
        if success {
            entry.record_success();
        } else {
            entry.record_failure();
        }
    }

    /// Snapshot every known agent's circuit breaker.
    pub fn all_breakers(&self) -> Vec<(String, CircuitBreaker)> {
        self.breakers
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect()
    }

    // ---- memory ---------------------------------------------------------

    /// Record a token-usage snapshot and classify the resulting pressure.
    ///
    /// Why: the MCP `memory_protect` tool and `TokenUsageUpdate` hooks both
    /// feed usage in; the daemon stores it and returns the pressure level so
    /// the caller (and dashboard) know whether to warn/alert/compact.
    /// What: stores `usage` for the session, returns `usage.pressure(config)`.
    /// Test: `memory_pressure_is_classified`.
    pub fn record_memory(&self, session: SessionId, usage: MemoryUsage) -> MemoryPressure {
        self.memory.insert(session, usage);
        usage.pressure(&self.memory_config)
    }

    /// Latest memory usage for a session, if any has been recorded.
    pub fn memory_for(&self, session: SessionId) -> Option<MemoryUsage> {
        self.memory.get(&session).map(|e| *e.value())
    }

    // ---- hook events ----------------------------------------------------

    /// Append a hook event to the bounded history ring buffer.
    ///
    /// Why: the dashboard's live feed reads recent events; the buffer must not
    /// grow without bound in a long-running daemon.
    /// What: pushes to the back, evicting the oldest once `HOOK_HISTORY_LIMIT`
    /// is exceeded.
    /// Test: `hook_history_is_bounded`.
    pub fn push_hook_event(&self, record: HookEventRecord) {
        let mut buf = self.hook_history.lock();
        if buf.len() >= HOOK_HISTORY_LIMIT {
            buf.pop_front();
        }
        buf.push_back(record);
    }

    /// Snapshot recent hook events, newest last.
    pub fn recent_hook_events(&self) -> Vec<HookEventRecord> {
        self.hook_history.lock().iter().cloned().collect()
    }

    /// Recent hook events for one session only.
    pub fn hook_events_for(&self, session: SessionId) -> Vec<HookEventRecord> {
        self.hook_history
            .lock()
            .iter()
            .filter(|r| r.session == session)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trusty_mpm_core::hook::HookEvent;
    use trusty_mpm_core::session::{ControlModel, SessionStatus};

    fn sample_session() -> Session {
        Session {
            id: SessionId::new(),
            workdir: "/tmp/p".into(),
            status: SessionStatus::Active,
            control: ControlModel::Tmux,
            active_delegations: 0,
        }
    }

    #[test]
    fn register_and_list_sessions() {
        let state = DaemonState::new();
        let s = sample_session();
        let id = s.id;
        state.register_session(s);
        assert_eq!(state.list_sessions().len(), 1);
        assert!(state.session(id).is_some());
        assert!(state.remove_session(id).is_some());
        assert!(state.list_sessions().is_empty());
    }

    #[test]
    fn breaker_tracks_outcomes() {
        let state = DaemonState::new();
        // Default threshold is 3 consecutive failures.
        for _ in 0..3 {
            state.record_outcome("research", false);
        }
        let cb = state.breaker("research");
        assert!(!cb.allows_delegation());
        // A success resets the counter (after an attempt_reset path it closes).
        state.record_outcome("research", true);
        assert_eq!(state.breaker("research").consecutive_failures, 0);
    }

    #[test]
    fn memory_pressure_is_classified() {
        let state = DaemonState::new();
        let id = SessionId::new();
        let pressure = state.record_memory(
            id,
            MemoryUsage {
                used_tokens: 900,
                window_tokens: 1000,
            },
        );
        assert_eq!(pressure, MemoryPressure::Compact);
        assert!(state.memory_for(id).is_some());
    }

    #[test]
    fn hook_history_is_bounded() {
        let state = DaemonState::new();
        let id = SessionId::new();
        for _ in 0..(HOOK_HISTORY_LIMIT + 50) {
            state.push_hook_event(HookEventRecord::now(
                id,
                HookEvent::PreToolUse,
                serde_json::Value::Null,
            ));
        }
        assert_eq!(state.recent_hook_events().len(), HOOK_HISTORY_LIMIT);
        assert_eq!(state.hook_events_for(id).len(), HOOK_HISTORY_LIMIT);
    }
}
