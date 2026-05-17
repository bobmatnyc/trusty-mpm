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

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::{Mutex, RwLock};
use trusty_mpm_core::agent::Delegation;
use trusty_mpm_core::circuit::{CircuitBreaker, CircuitConfig};
use trusty_mpm_core::deterministic_overseer::DeterministicOverseer;
use trusty_mpm_core::hook::HookEventRecord;
use trusty_mpm_core::memory::{MemoryConfig, MemoryPressure, MemoryUsage};
use trusty_mpm_core::overseer::Overseer;
use trusty_mpm_core::overseer_config::OverseerConfig;
use trusty_mpm_core::paths::FrameworkPaths;
use trusty_mpm_core::project::ProjectInfo;
use trusty_mpm_core::session::{Session, SessionId};

use crate::audit::AuditLogger;
use crate::optimizer::OptimizerConfig;
use crate::tmux::TmuxDriver;

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
    /// Discovered trusty sidecar service addresses, set once at startup.
    trusty_addrs: Mutex<Option<crate::discover::TrustyAddrs>>,
    /// Token-use optimizer config; read on every PostToolUse, updatable at
    /// runtime via the HTTP API, hence behind an `RwLock`.
    optimizer: Arc<parking_lot::RwLock<OptimizerConfig>>,
    /// Registered projects, keyed by their absolute working-directory path.
    ///
    /// Why: sessions are grouped by project; the `project` subcommands and the
    /// dashboard read this registry. An `RwLock<HashMap>` suits a low-churn
    /// registry that is read far more often than written.
    projects: Arc<RwLock<HashMap<PathBuf, ProjectInfo>>>,
    /// Session overseer — evaluates hook events for allow/block/respond/flag.
    ///
    /// Why: oversight is a pluggable strategy; the daemon holds it behind
    /// `dyn Overseer` so the deterministic and LLM implementations are
    /// interchangeable. Opt-in: a disabled overseer fast-paths every call.
    overseer: Arc<dyn Overseer>,
    /// Name of the active overseer strategy, for the `GET /overseer` endpoint
    /// and the audit log (`"deterministic"` or `"composite-llm"`).
    overseer_handler: String,
    /// Append-only JSONL logger for every overseer decision.
    audit: Arc<AuditLogger>,
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

/// Read the optimizer policy from the installed framework, never failing.
///
/// Why: daemon startup must not abort because the framework is not installed
/// or its policy file is malformed; a sensible default keeps the daemon usable.
/// What: loads `~/.trusty-mpm/framework/hooks/optimizer.toml` via
/// [`OptimizerConfig::load_from_file`], logging and falling back to
/// `OptimizerConfig::default()` on any error.
/// Test: `new_reads_default_when_optimizer_file_missing`,
/// `reload_optimizer_config_picks_up_file_changes`.
fn load_optimizer_config() -> OptimizerConfig {
    let path = FrameworkPaths::default().optimizer_config();
    match OptimizerConfig::load_from_file(&path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(
                "failed to load optimizer config from {}: {e}; using defaults",
                path.display()
            );
            OptimizerConfig::default()
        }
    }
}

/// Build the session overseer from the installed framework policy.
///
/// Why: oversight is framework-managed and opt-in; daemon startup must reflect
/// `~/.trusty-mpm/framework/hooks/overseer.toml` (or a safe disabled default
/// when it is absent) without ever failing to construct.
/// What: loads [`OverseerConfig`] from [`FrameworkPaths::overseer_config`] and
/// builds the overseer via [`build_overseer`]; a missing/malformed file yields
/// the disabled default config (handled inside `OverseerConfig::load_from`).
/// Test: `new_overseer_is_disabled_when_file_missing`.
fn load_overseer() -> (Arc<dyn Overseer>, String) {
    let path = FrameworkPaths::default().overseer_config();
    build_overseer(OverseerConfig::load_from(&path))
}

/// Assemble the overseer strategy from a loaded [`OverseerConfig`].
///
/// Why: the daemon may run rule-based oversight alone, or compose it with the
/// LLM overseer when `[llm] enabled = true` *and* an API key is present.
/// Deciding the strategy in one place keeps `new()` / `with_paths()` aligned.
/// What: always builds a [`DeterministicOverseer`]; when the LLM section is
/// enabled and the configured API key resolves, wraps both in a
/// [`CompositeOverseer`] (deterministic first, LLM for uncertain cases).
/// Returns the overseer and its handler name (`"deterministic"` or
/// `"composite-llm"`).
/// Test: `overseer_is_deterministic_without_llm`,
/// `overseer_falls_back_when_llm_key_missing`.
fn build_overseer(config: OverseerConfig) -> (Arc<dyn Overseer>, String) {
    let deterministic = DeterministicOverseer::new(config.clone());
    if config.llm.enabled {
        let llm = crate::llm_overseer::LlmOverseer::new(
            config.llm.model.clone(),
            &config.llm.api_key_env,
        );
        if llm.is_enabled() {
            tracing::info!(
                "LLM overseer active (model {}); composing with deterministic rules",
                config.llm.model
            );
            let composite = crate::overseer_compose::CompositeOverseer::new(
                Box::new(deterministic),
                Box::new(llm),
            );
            return (Arc::new(composite), "composite-llm".to_string());
        }
        tracing::warn!(
            "[llm] enabled but no API key in ${}; falling back to deterministic overseer",
            config.llm.api_key_env
        );
    }
    (Arc::new(deterministic), "deterministic".to_string())
}

/// Resolve the daemon's logs directory (`~/.trusty-mpm/logs`).
///
/// Why: the audit logger writes under a single well-known directory; resolving
/// it via the home directory keeps it consistent with the framework root.
/// What: returns `<home>/.trusty-mpm/logs`, falling back to `./.trusty-mpm/logs`
/// when the home directory cannot be determined.
/// Test: exercised indirectly by `new_builds_audit_logger`.
fn logs_dir() -> PathBuf {
    FrameworkPaths::default().root.join("logs")
}

impl DaemonState {
    /// Construct empty state with default thresholds.
    ///
    /// Why: the optimizer and overseer policies are framework-managed on disk
    /// (`~/.trusty-mpm/framework/hooks/`); the daemon must reflect whatever the
    /// installed framework declares without an API round-trip.
    /// What: reads the optimizer config from
    /// [`FrameworkPaths::optimizer_config`] and the overseer policy from
    /// [`FrameworkPaths::overseer_config`], falling back to safe defaults when
    /// either file is missing (framework not yet installed) or unparseable
    /// (logged, not fatal); builds the audit logger under `~/.trusty-mpm/logs`.
    /// Test: `new_reads_default_when_optimizer_file_missing`,
    /// `new_overseer_is_disabled_when_file_missing`.
    pub fn new() -> Self {
        let optimizer = load_optimizer_config();
        let (overseer, overseer_handler) = load_overseer();
        Self {
            sessions: DashMap::new(),
            delegations: DashMap::new(),
            breakers: DashMap::new(),
            memory: DashMap::new(),
            hook_history: Mutex::new(VecDeque::with_capacity(HOOK_HISTORY_LIMIT)),
            memory_config: MemoryConfig::default(),
            circuit_config: CircuitConfig::default(),
            trusty_addrs: Mutex::new(None),
            optimizer: Arc::new(parking_lot::RwLock::new(optimizer)),
            projects: Arc::new(RwLock::new(HashMap::new())),
            overseer,
            overseer_handler,
            audit: Arc::new(AuditLogger::new(&logs_dir())),
        }
    }

    /// Wrap the state in an `Arc` for sharing across tasks.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Construct state whose framework-managed config is read from `paths`.
    ///
    /// Why: [`DaemonState::new`] reads the optimizer / overseer policy and the
    /// audit log location from the real `~/.trusty-mpm` install. End-to-end
    /// tests must point those reads at a hermetic temp directory instead so a
    /// test never touches (or depends on) the operator's real framework. This
    /// constructor takes an explicit [`FrameworkPaths`] — typically built with
    /// [`FrameworkPaths::under`] against a `tempfile::TempDir`.
    /// What: loads `optimizer.toml` / `overseer.toml` from `paths.hooks` and
    /// builds the audit logger under `paths.root/logs`, falling back to safe
    /// defaults exactly as [`DaemonState::new`] does when a file is absent.
    /// Test: the `e2e` integration suite (`test_optimizer`, `test_overseer`).
    pub fn with_paths(paths: &FrameworkPaths) -> Self {
        let optimizer = match OptimizerConfig::load_from_file(&paths.optimizer_config()) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!("failed to load optimizer config: {e}; using defaults");
                OptimizerConfig::default()
            }
        };
        let overseer_cfg = OverseerConfig::load_from(&paths.overseer_config());
        let (overseer, overseer_handler) = build_overseer(overseer_cfg);
        Self {
            sessions: DashMap::new(),
            delegations: DashMap::new(),
            breakers: DashMap::new(),
            memory: DashMap::new(),
            hook_history: Mutex::new(VecDeque::with_capacity(HOOK_HISTORY_LIMIT)),
            memory_config: MemoryConfig::default(),
            circuit_config: CircuitConfig::default(),
            trusty_addrs: Mutex::new(None),
            optimizer: Arc::new(parking_lot::RwLock::new(optimizer)),
            projects: Arc::new(RwLock::new(HashMap::new())),
            overseer,
            overseer_handler,
            audit: Arc::new(AuditLogger::new(&paths.root.join("logs"))),
        }
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

    /// Mutate an existing session in place under a write lock.
    ///
    /// Why: the pause/resume handlers must change a session's `status`,
    /// `paused_at`, and `pause_summary` atomically without the read-modify-write
    /// race of `session()` + `register_session()`.
    /// What: takes a write guard on the session entry and calls `f` if the
    /// session exists; returns `true` when it ran, `false` for an unknown id.
    /// Test: `update_session_mutates_existing`, `update_session_missing_is_false`.
    pub fn update_session<F>(&self, id: &SessionId, f: F) -> bool
    where
        F: FnOnce(&mut Session),
    {
        match self.sessions.get_mut(id) {
            Some(mut entry) => {
                f(entry.value_mut());
                true
            }
            None => false,
        }
    }

    /// Snapshot the sessions belonging to one project.
    ///
    /// Why: `GET /sessions?project=<path>` and `trusty-mpm session list`
    /// scope the listing to the caller's project.
    /// What: returns every session whose `project_path` equals `path`.
    /// Test: `list_sessions_for_project_filters`.
    pub fn list_sessions_for_project(&self, path: &std::path::Path) -> Vec<Session> {
        self.sessions
            .iter()
            .filter(|e| e.value().project_path.as_deref() == Some(path))
            .map(|e| e.value().clone())
            .collect()
    }

    /// Look up one session by id or by friendly tmux name.
    ///
    /// Why: the `session stop` / `session info` subcommands accept either a
    /// UUID or the friendly `tmpm-<adj>-<noun>` name the daemon prints on
    /// start; resolving both keeps the CLI ergonomic.
    /// What: tries to parse `key` as a UUID first; on failure scans the
    /// registry for a session whose `tmux_name` matches.
    /// Test: `find_session_by_id_or_name`.
    pub fn find_session(&self, key: &str) -> Option<Session> {
        if let Ok(uuid) = uuid::Uuid::parse_str(key) {
            return self.session(SessionId(uuid));
        }
        self.sessions
            .iter()
            .find(|e| e.value().tmux_name == key)
            .map(|e| e.value().clone())
    }

    /// Drop registry entries whose tmux session no longer exists.
    ///
    /// Why: sessions accumulate forever otherwise — a dead tmux session leaves a
    /// stale registry entry behind. The daemon's housekeeping loop calls this
    /// periodically, and `DELETE /sessions/dead` calls it on demand.
    /// What: discovers the live tmux session names via `driver.list_sessions()`,
    /// then removes any session whose `tmux_name` is absent from that set;
    /// returns the number reaped. A failed tmux listing reaps nothing (returns
    /// `0`) rather than wrongly deleting every session.
    /// Test: `reap_dead_sessions`.
    pub fn reap_dead_sessions(&self, driver: &TmuxDriver) -> usize {
        let live: std::collections::HashSet<String> = match driver.list_sessions() {
            Ok(sessions) => sessions.into_iter().map(|s| s.name).collect(),
            Err(e) => {
                tracing::warn!("reap skipped — tmux list-sessions failed: {e}");
                return 0;
            }
        };
        self.reap_against(&live)
    }

    /// Remove every session whose `tmux_name` is not in `live`.
    ///
    /// Why: separating the set-difference logic from the tmux call makes the
    /// reaping rule unit-testable without spawning a tmux process.
    /// What: collects the dead ids, then removes each; returns the count.
    /// Test: `reap_dead_sessions`.
    fn reap_against(&self, live: &std::collections::HashSet<String>) -> usize {
        let dead: Vec<SessionId> = self
            .sessions
            .iter()
            .filter(|e| !live.contains(&e.value().tmux_name))
            .map(|e| *e.key())
            .collect();
        for id in &dead {
            self.remove_session(*id);
        }
        dead.len()
    }

    // ---- projects -------------------------------------------------------

    /// Register a project by its working-directory path.
    ///
    /// Why: `trusty-mpm project init` and `POST /projects` need to record a
    /// directory as a managed project so sessions can be associated with it.
    /// What: builds a [`ProjectInfo`] from `path`, inserting (or replacing) it
    /// in the registry keyed by the path; returns the stored info.
    /// Test: `register_and_list_projects`.
    pub fn register_project(&self, path: PathBuf) -> ProjectInfo {
        let info = ProjectInfo::new(path.clone());
        self.projects.write().insert(path, info.clone());
        info
    }

    /// Snapshot every registered project.
    ///
    /// Why: `trusty-mpm project list` and `GET /projects` need the full set.
    /// What: clones each [`ProjectInfo`] out from under a short read lock.
    /// Test: `register_and_list_projects`.
    pub fn list_projects(&self) -> Vec<ProjectInfo> {
        self.projects.read().values().cloned().collect()
    }

    /// Look up one registered project by its path.
    ///
    /// Why: `GET /projects/current` resolves the project for the caller's cwd.
    /// What: returns a clone of the stored [`ProjectInfo`], or `None` if the
    /// path is not registered.
    /// Test: `project_lookup_by_path`.
    pub fn project(&self, path: &std::path::Path) -> Option<ProjectInfo> {
        self.projects.read().get(path).cloned()
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

    // ---- trusty sidecar discovery --------------------------------------

    /// Record the trusty sidecar addresses discovered at daemon startup.
    ///
    /// Why: discovery runs once when the HTTP daemon boots; the resolved
    /// addresses must be visible to request handlers that proxy to the
    /// trusty-memory / trusty-search sidecars.
    /// What: stores the `TrustyAddrs` snapshot under the mutex.
    /// Test: `trusty_addrs_round_trip`.
    pub fn set_trusty_addrs(&self, addrs: crate::discover::TrustyAddrs) {
        *self.trusty_addrs.lock() = Some(addrs);
    }

    /// Read the discovered trusty sidecar addresses, if discovery has run.
    ///
    /// Why: handlers need the resolved addresses; `None` means discovery has
    /// not completed (e.g. in MCP mode, which skips it).
    /// What: returns a clone of the stored `TrustyAddrs`.
    /// Test: `trusty_addrs_round_trip`.
    #[allow(dead_code)] // Read by sidecar-proxy handlers landing in a follow-up.
    pub fn trusty_addrs(&self) -> Option<crate::discover::TrustyAddrs> {
        self.trusty_addrs.lock().clone()
    }

    // ---- token-use optimizer -------------------------------------------

    /// Snapshot the current optimizer configuration.
    ///
    /// Why: the PostToolUse hook path reads this on every event; cloning a
    /// small struct under a short read lock keeps the hot path lock-free
    /// during compression itself.
    /// What: returns a clone of the stored `OptimizerConfig`.
    /// Test: `get_optimizer_returns_default`.
    pub fn optimizer_config(&self) -> OptimizerConfig {
        self.optimizer.read().clone()
    }

    /// Re-read the optimizer policy from the installed framework on disk.
    ///
    /// Why: the policy file is framework-managed and edited directly (or reset
    /// via `trusty-mpm install --force`); the file watcher calls this when
    /// `optimizer.toml` changes so the running daemon picks up edits without a
    /// restart.
    /// What: reloads `~/.trusty-mpm/framework/hooks/optimizer.toml`, replacing
    /// the in-memory config under a write lock. A missing or malformed file
    /// falls back to `OptimizerConfig::default()` (logged, not fatal).
    /// Test: `reload_optimizer_config_picks_up_file_changes`.
    pub fn reload_optimizer_config(&self) {
        *self.optimizer.write() = load_optimizer_config();
    }

    /// Reload the optimizer policy from an explicit file path.
    ///
    /// Why: tests must exercise the reload path against a temp file without
    /// touching the real `~/.trusty-mpm` framework install.
    /// What: loads `path` via [`OptimizerConfig::load_from_file`] and stores the
    /// result; a missing file yields `OptimizerConfig::default()`.
    /// Test: `reload_optimizer_config_picks_up_file_changes`.
    pub fn reload_optimizer_config_from(&self, path: &std::path::Path) -> anyhow::Result<()> {
        let cfg = OptimizerConfig::load_from_file(path)?;
        *self.optimizer.write() = cfg;
        Ok(())
    }

    // ---- overseer -------------------------------------------------------

    /// The session overseer for evaluating hook events.
    ///
    /// Why: the hook relay consults the overseer on tool-use events; handing
    /// out the shared `Arc` keeps every call site using the one configured
    /// strategy.
    /// What: returns a clone of the `Arc<dyn Overseer>`.
    /// Test: `overseer_is_accessible`.
    pub fn overseer(&self) -> Arc<dyn Overseer> {
        Arc::clone(&self.overseer)
    }

    /// Name of the active overseer strategy.
    ///
    /// Why: `GET /overseer` and the audit log report which strategy is in
    /// force; the name is fixed at construction so callers need no config.
    /// What: returns `"deterministic"` or `"composite-llm"`.
    /// Test: `overseer_handler_reports_strategy`.
    pub fn overseer_handler(&self) -> &str {
        &self.overseer_handler
    }

    /// The overseer audit logger.
    ///
    /// Why: the hook relay logs every overseer decision; sharing the `Arc`
    /// keeps all decisions flowing into the one dated JSONL file.
    /// What: returns a clone of the `Arc<AuditLogger>`.
    /// Test: `audit_logger_is_accessible`.
    pub fn audit(&self) -> Arc<AuditLogger> {
        Arc::clone(&self.audit)
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
        let mut s = Session::new(SessionId::new(), "/tmp/p", ControlModel::Tmux);
        s.status = SessionStatus::Active;
        s
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
    fn update_session_mutates_existing() {
        let state = DaemonState::new();
        let s = sample_session();
        let id = s.id;
        state.register_session(s);
        let ran = state.update_session(&id, |session| {
            session.status = SessionStatus::Paused;
            session.pause_summary = Some("note".to_string());
        });
        assert!(ran);
        let updated = state.session(id).expect("session exists");
        assert_eq!(updated.status, SessionStatus::Paused);
        assert_eq!(updated.pause_summary.as_deref(), Some("note"));
    }

    #[test]
    fn update_session_missing_is_false() {
        let state = DaemonState::new();
        let ran = state.update_session(&SessionId::new(), |_| {});
        assert!(!ran);
    }

    #[test]
    fn register_and_list_projects() {
        let state = DaemonState::new();
        assert!(state.list_projects().is_empty());
        let info = state.register_project(PathBuf::from("/work/demo"));
        assert_eq!(info.name, "demo");
        assert_eq!(state.list_projects().len(), 1);
        // Re-registering the same path replaces rather than duplicates.
        state.register_project(PathBuf::from("/work/demo"));
        assert_eq!(state.list_projects().len(), 1);
        state.register_project(PathBuf::from("/work/other"));
        assert_eq!(state.list_projects().len(), 2);
    }

    #[test]
    fn project_lookup_by_path() {
        let state = DaemonState::new();
        state.register_project(PathBuf::from("/work/demo"));
        assert!(state.project(std::path::Path::new("/work/demo")).is_some());
        assert!(
            state
                .project(std::path::Path::new("/work/missing"))
                .is_none()
        );
    }

    #[test]
    fn list_sessions_for_project_filters() {
        let state = DaemonState::new();
        let mut in_proj = sample_session();
        in_proj.project_path = Some(PathBuf::from("/work/demo"));
        let mut other_proj = sample_session();
        other_proj.project_path = Some(PathBuf::from("/work/other"));
        let no_proj = sample_session();
        state.register_session(in_proj.clone());
        state.register_session(other_proj);
        state.register_session(no_proj);

        let listed = state.list_sessions_for_project(std::path::Path::new("/work/demo"));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, in_proj.id);
    }

    #[test]
    fn find_session_by_id_or_name() {
        let state = DaemonState::new();
        let s = sample_session();
        let id = s.id;
        let name = s.tmux_name.clone();
        state.register_session(s);

        assert!(state.find_session(&id.0.to_string()).is_some());
        assert!(state.find_session(&name).is_some());
        assert!(state.find_session("tmpm-no-such-name").is_none());
        assert!(
            state
                .find_session(&SessionId::new().0.to_string())
                .is_none()
        );
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
    fn trusty_addrs_round_trip() {
        let state = DaemonState::new();
        assert!(state.trusty_addrs().is_none());
        let addrs = crate::discover::TrustyAddrs {
            memory: "127.0.0.1:3038".parse().unwrap(),
            search: "127.0.0.1:7878".parse().unwrap(),
        };
        state.set_trusty_addrs(addrs);
        let got = state.trusty_addrs().expect("addrs stored");
        assert_eq!(got.memory, "127.0.0.1:3038".parse().unwrap());
        assert_eq!(got.search, "127.0.0.1:7878".parse().unwrap());
    }

    #[test]
    fn reap_dead_sessions() {
        // Three registered sessions; tmux reports only two of them alive.
        // `reap_against` (the testable core of `reap_dead_sessions`) must drop
        // exactly the one whose tmux_name is absent from the live set.
        let state = DaemonState::new();
        let alive_a = sample_session();
        let alive_b = sample_session();
        let dead = sample_session();
        let (id_a, id_b, id_dead) = (alive_a.id, alive_b.id, dead.id);
        state.register_session(alive_a.clone());
        state.register_session(alive_b.clone());
        state.register_session(dead);
        assert_eq!(state.list_sessions().len(), 3);

        let live: std::collections::HashSet<String> =
            [alive_a.tmux_name.clone(), alive_b.tmux_name.clone()]
                .into_iter()
                .collect();
        let removed = state.reap_against(&live);

        assert_eq!(removed, 1);
        assert!(state.session(id_a).is_some());
        assert!(state.session(id_b).is_some());
        assert!(state.session(id_dead).is_none());

        // Reaping again is idempotent — nothing left to remove.
        assert_eq!(state.reap_against(&live), 0);
    }

    #[test]
    fn reap_against_empty_live_removes_all() {
        // An empty live set (e.g. tmux server fully stopped) drops every entry.
        let state = DaemonState::new();
        state.register_session(sample_session());
        state.register_session(sample_session());
        let removed = state.reap_against(&std::collections::HashSet::new());
        assert_eq!(removed, 2);
        assert!(state.list_sessions().is_empty());
    }

    #[test]
    fn new_reads_default_when_optimizer_file_missing() {
        // With no framework installed (the optimizer.toml file absent), the
        // daemon must still construct, falling back to the default policy.
        let state = DaemonState::new();
        assert_eq!(
            state.optimizer_config().default_level,
            trusty_mpm_core::compress::CompressionLevel::Trim
        );
    }

    #[test]
    fn reload_optimizer_config_picks_up_file_changes() {
        // Reloading from an explicit temp file must overwrite the in-memory
        // policy with whatever the file declares.
        use std::io::Write;
        let state = DaemonState::new();
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("optimizer.toml");
        let mut file = std::fs::File::create(&path).expect("create file");
        writeln!(file, "[default]\nlevel = \"caveman\"").expect("write file");

        state
            .reload_optimizer_config_from(&path)
            .expect("reload succeeds");
        assert_eq!(
            state.optimizer_config().default_level,
            trusty_mpm_core::compress::CompressionLevel::Caveman
        );

        // A missing file reloads to the default policy rather than erroring.
        state
            .reload_optimizer_config_from(&dir.path().join("absent.toml"))
            .expect("missing file is not an error");
        assert_eq!(
            state.optimizer_config().default_level,
            trusty_mpm_core::compress::CompressionLevel::Trim
        );
    }

    #[test]
    fn new_overseer_is_disabled_when_file_missing() {
        // With no framework installed (overseer.toml absent), the overseer
        // must be present but disabled — oversight is opt-in.
        let state = DaemonState::new();
        assert!(!state.overseer().is_enabled());
    }

    #[test]
    fn overseer_is_deterministic_without_llm() {
        // With the `[llm]` section absent/disabled, the overseer is the plain
        // deterministic strategy and (with no rules) reports disabled.
        let cfg = OverseerConfig::default();
        let (overseer, handler) = build_overseer(cfg);
        assert!(!overseer.is_enabled());
        assert_eq!(handler, "deterministic");
    }

    #[test]
    fn overseer_falls_back_when_llm_key_missing() {
        // `[llm] enabled = true` but no API key resolves: the daemon must not
        // panic — it falls back to the deterministic overseer.
        let mut cfg = OverseerConfig::default();
        cfg.llm.enabled = true;
        cfg.llm.api_key_env = "TRUSTY_MPM_DEFINITELY_NOT_SET".to_string(); // pragma: allowlist secret
        let (overseer, handler) = build_overseer(cfg);
        // Deterministic with no rules and disabled top-level flag → disabled.
        assert!(!overseer.is_enabled());
        assert_eq!(handler, "deterministic");
    }

    #[test]
    fn overseer_handler_reports_strategy() {
        // The default daemon reports the deterministic handler.
        let state = DaemonState::new();
        assert_eq!(state.overseer_handler(), "deterministic");
    }

    #[test]
    fn overseer_is_accessible() {
        let state = DaemonState::new();
        // The shared overseer can be cloned out and queried.
        let overseer = state.overseer();
        assert!(!overseer.is_enabled());
    }

    #[test]
    fn audit_logger_is_accessible() {
        let state = DaemonState::new();
        // The audit logger resolves a dated JSONL path under `logs/overseer`.
        let audit = state.audit();
        assert_eq!(
            audit.path().extension().and_then(|e| e.to_str()),
            Some("jsonl")
        );
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
