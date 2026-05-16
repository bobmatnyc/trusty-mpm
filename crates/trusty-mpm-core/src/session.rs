//! Session state model.
//!
//! Why: The daemon, TUI, and Telegram bot all need a shared view of what a
//! Claude Code session is and what state it is in.
//! What: Defines `SessionId`, `SessionStatus`, and the `Session` snapshot type
//! exchanged over IPC.
//! Test: `cargo test -p trusty-mpm-core` round-trips a `Session` through JSON.

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_json_roundtrip() {
        let session = Session {
            id: SessionId::new(),
            workdir: "/tmp/project".into(),
            status: SessionStatus::Active,
            control: ControlModel::Tmux,
            active_delegations: 2,
        };
        let json = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, session.id);
        assert_eq!(back.active_delegations, 2);
    }
}
