//! Session state model.
//!
//! Why: The daemon, TUI, and Telegram bot all need a shared view of what a
//! Claude Code session is and what state it is in.
//! What: Defines `SessionId`, `SessionStatus`, and the `Session` snapshot type
//! exchanged over IPC.
//! Test: `cargo test -p trusty-mpm-core` round-trips a `Session` through JSON.

use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable identifier for a managed session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub Uuid);

impl SessionId {
    /// Generate a fresh random session id.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Lifecycle state of a managed Claude Code session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    /// Session process is spawning.
    Starting,
    /// Session is running and accepting input.
    Active,
    /// Session is blocked awaiting a permission decision.
    AwaitingApproval,
    /// Session has been detached but the process is still alive.
    Detached,
    /// Session process has exited.
    Stopped,
}

/// Control model used to host a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlModel {
    /// Session runs inside a named tmux session.
    Tmux,
    /// Session runs under a daemon-owned PTY.
    Pty,
    /// Session runs non-interactively via the Claude Code SDK / headless mode.
    Sdk,
}

/// A point-in-time snapshot of a session, returned by the daemon API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique session id.
    pub id: SessionId,
    /// Working directory the session was launched in.
    pub workdir: String,
    /// Current lifecycle status.
    pub status: SessionStatus,
    /// How the session is hosted.
    pub control: ControlModel,
    /// Number of active agent delegations within the session.
    pub active_delegations: u32,
    /// Friendly tmux session name (`tmpm-<adjective>-<noun>`).
    ///
    /// Why: the daemon's reaper compares this against the live tmux session
    /// list, and the dashboard shows it instead of the raw UUID.
    #[serde(default)]
    pub tmux_name: String,
    /// When the session was registered with the daemon.
    #[serde(default = "SystemTime::now")]
    pub created_at: SystemTime,
    /// When the session was last observed alive (heartbeat / activity).
    #[serde(default = "SystemTime::now")]
    pub last_seen: SystemTime,
    /// The trusty-mpm project this session belongs to, if any.
    ///
    /// Why: a session is started inside a registered project; recording the
    /// project root lets the CLI and dashboard filter sessions per project.
    /// `None` for sessions started outside any registered project.
    #[serde(default)]
    pub project_path: Option<PathBuf>,
}

impl Session {
    /// Build a freshly-registered session with derived metadata.
    ///
    /// Why: every call site that creates a `Session` needs the same defaults —
    /// a friendly tmux name derived from the id and `created_at`/`last_seen`
    /// stamped to now; centralizing it prevents drift.
    /// What: derives `tmux_name` via [`crate::names::name_from_uuid`] and stamps
    /// both timestamps to the current time.
    /// Test: `new_derives_tmux_name`.
    pub fn new(id: SessionId, workdir: impl Into<String>, control: ControlModel) -> Self {
        let now = SystemTime::now();
        Self {
            id,
            workdir: workdir.into(),
            status: SessionStatus::Starting,
            control,
            active_delegations: 0,
            tmux_name: crate::names::name_from_uuid(&id.0),
            created_at: now,
            last_seen: now,
            project_path: None,
        }
    }

    /// Mark the session as observed alive right now.
    ///
    /// Why: the reaper and dashboard use `last_seen` to distinguish active from
    /// stale sessions; heartbeats and activity must refresh it.
    /// What: sets `last_seen` to the current time.
    /// Test: `touch_advances_last_seen`.
    pub fn touch(&mut self) {
        self.last_seen = SystemTime::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_json_roundtrip() {
        let mut session = Session::new(SessionId::new(), "/tmp/project", ControlModel::Tmux);
        session.status = SessionStatus::Active;
        session.active_delegations = 2;
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, session.id);
        assert_eq!(back.active_delegations, 2);
        assert_eq!(back.tmux_name, session.tmux_name);
    }

    #[test]
    fn new_derives_tmux_name() {
        let id = SessionId::new();
        let session = Session::new(id, "/tmp/p", ControlModel::Tmux);
        assert_eq!(session.tmux_name, crate::names::name_from_uuid(&id.0));
        assert!(session.tmux_name.starts_with("tmpm-"));
        assert_eq!(session.status, SessionStatus::Starting);
    }

    #[test]
    fn new_has_no_project_by_default() {
        let session = Session::new(SessionId::new(), "/tmp/p", ControlModel::Tmux);
        assert_eq!(session.project_path, None);
    }

    #[test]
    fn project_path_survives_json_roundtrip() {
        let mut session = Session::new(SessionId::new(), "/tmp/p", ControlModel::Tmux);
        session.project_path = Some(std::path::PathBuf::from("/work/proj"));
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.project_path,
            Some(std::path::PathBuf::from("/work/proj"))
        );
    }

    #[test]
    fn touch_advances_last_seen() {
        let mut session = Session::new(SessionId::new(), "/tmp/p", ControlModel::Tmux);
        let before = session.last_seen;
        std::thread::sleep(std::time::Duration::from_millis(2));
        session.touch();
        assert!(session.last_seen >= before);
    }
}
