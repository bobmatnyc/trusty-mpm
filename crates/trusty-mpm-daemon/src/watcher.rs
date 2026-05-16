//! Multi-session file monitoring.
//!
//! Why: the dashboard's file panel watches project files for changes across
//! every active session — a multi-session superset of claude-mpm's single
//! file watcher. When a watched file changes the daemon synthesises a
//! `FileChanged` hook event so the change shows up in the same live feed as
//! every other event.
//! What: [`FileWatcher`] registers watch roots (one per session workdir) with
//! the `notify` crate and converts filesystem events into `HookEventRecord`s
//! on the shared [`DaemonState`].
//! Test: `cargo test -p trusty-mpm-daemon` checks watch-root bookkeeping and
//! the path-to-event conversion without needing real filesystem events.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use notify::{EventKind, RecursiveMode, Watcher, recommended_watcher};
use parking_lot::Mutex;
use tracing::{debug, info, warn};
use trusty_mpm_core::hook::{HookEvent, HookEventRecord};
use trusty_mpm_core::session::SessionId;

use crate::state::DaemonState;

/// Watches session working directories and feeds change events to the daemon.
///
/// Why: keeping the watch-root registry and the event-synthesis logic in one
/// type makes the dashboard's file panel a thin consumer of `DaemonState`.
/// What: holds the shared state plus a map of session → watched root. The
/// `notify` watcher itself is created in [`FileWatcher::spawn`]; this struct
/// owns the bookkeeping that is unit-testable.
/// Test: `register_and_unregister_roots`, `synthesises_file_changed_event`.
pub struct FileWatcher {
    /// Shared daemon state the synthesised events are pushed onto.
    state: Arc<DaemonState>,
    /// Session id → the directory being watched for that session.
    roots: Mutex<HashMap<SessionId, PathBuf>>,
}

impl FileWatcher {
    /// Create a watcher bound to shared daemon state.
    pub fn new(state: Arc<DaemonState>) -> Self {
        Self {
            state,
            roots: Mutex::new(HashMap::new()),
        }
    }

    /// Register a directory to watch on behalf of a session.
    ///
    /// Why: each session has its own workdir; the dashboard shows file changes
    /// per session, so the watcher must know which root maps to which session.
    /// What: records the `session → root` mapping; returns the previous root
    /// if the session was already watching something.
    /// Test: `register_and_unregister_roots`.
    pub fn watch_session(&self, session: SessionId, root: PathBuf) -> Option<PathBuf> {
        self.roots.lock().insert(session, root)
    }

    /// Stop watching a session's directory (called on session teardown).
    #[allow(dead_code)] // Wired in the session-teardown milestone.
    pub fn unwatch_session(&self, session: SessionId) -> Option<PathBuf> {
        self.roots.lock().remove(&session)
    }

    /// Number of sessions currently being watched.
    pub fn watched_count(&self) -> usize {
        self.roots.lock().len()
    }

    /// Find which watched session a changed path belongs to.
    ///
    /// Why: `notify` reports an absolute path; the daemon must attribute the
    /// change to the right session before synthesising an event.
    /// What: returns the session whose watch root is a prefix of `path`. If
    /// several roots match (nested projects) the longest prefix wins.
    /// Test: `attributes_path_to_longest_matching_root`.
    pub fn session_for_path(&self, path: &std::path::Path) -> Option<SessionId> {
        let roots = self.roots.lock();
        roots
            .iter()
            .filter(|(_, root)| path.starts_with(root))
            .max_by_key(|(_, root)| root.as_os_str().len())
            .map(|(session, _)| *session)
    }

    /// Run the filesystem watcher loop until the daemon shuts down.
    ///
    /// Why: the daemon spawns this as a background task so file changes across
    /// every session's workdir flow into the shared hook feed.
    /// What: registers a `notify` watcher for each known session workdir, then
    /// drains filesystem events from a channel, attributing each changed path
    /// to a session via [`record_change`](Self::record_change).
    /// Test: bookkeeping and path attribution are unit-tested directly; this
    /// async glue is exercised by `cargo run`.
    pub async fn spawn(self) {
        // Seed watch roots from the sessions known at startup.
        for session in self.state.list_sessions() {
            let root = PathBuf::from(&session.workdir);
            if root.is_dir() {
                self.watch_session(session.id, root);
            }
        }

        // notify's callback is synchronous; bridge it onto a tokio channel.
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<PathBuf>();
        let mut watcher = match recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res
                && matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                )
            {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                warn!("file watcher unavailable: {e}");
                return;
            }
        };

        // Register every seeded watch root with the notify watcher.
        let roots: Vec<PathBuf> = self.roots.lock().values().cloned().collect();
        for root in &roots {
            if let Err(e) = watcher.watch(root, RecursiveMode::Recursive) {
                warn!("failed to watch {}: {e}", root.display());
            } else {
                debug!("watching {}", root.display());
            }
        }
        info!("file watcher started ({} root(s))", self.watched_count());

        // Drain change events for the lifetime of the daemon.
        while let Some(path) = rx.recv().await {
            if self.record_change(&path) {
                debug!("recorded file change: {}", path.display());
            }
        }
    }

    /// Synthesise and record a `FileChanged` hook event for a changed path.
    ///
    /// Why: routing file changes through the same hook pipeline means the
    /// dashboard feed, Telegram alerts, and history all treat them uniformly.
    /// What: attributes the path to a session, then pushes a `FileChanged`
    /// `HookEventRecord` carrying the path; returns `true` if attributed.
    /// Test: `synthesises_file_changed_event`.
    pub fn record_change(&self, path: &std::path::Path) -> bool {
        let Some(session) = self.session_for_path(path) else {
            return false;
        };
        let payload = serde_json::json!({ "path": path.to_string_lossy() });
        self.state.push_hook_event(HookEventRecord::now(
            session,
            HookEvent::FileChanged,
            payload,
        ));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_unregister_roots() {
        let watcher = FileWatcher::new(DaemonState::shared());
        let s = SessionId::new();
        assert_eq!(watcher.watched_count(), 0);
        assert!(
            watcher
                .watch_session(s, PathBuf::from("/tmp/proj"))
                .is_none()
        );
        assert_eq!(watcher.watched_count(), 1);
        assert_eq!(watcher.unwatch_session(s), Some(PathBuf::from("/tmp/proj")));
        assert_eq!(watcher.watched_count(), 0);
    }

    #[test]
    fn attributes_path_to_longest_matching_root() {
        let watcher = FileWatcher::new(DaemonState::shared());
        let outer = SessionId::new();
        let inner = SessionId::new();
        watcher.watch_session(outer, PathBuf::from("/tmp/proj"));
        watcher.watch_session(inner, PathBuf::from("/tmp/proj/sub"));
        // A file under the nested root attributes to the inner session.
        let hit = watcher.session_for_path(std::path::Path::new("/tmp/proj/sub/main.rs"));
        assert_eq!(hit, Some(inner));
        // A file only under the outer root attributes to the outer session.
        let hit = watcher.session_for_path(std::path::Path::new("/tmp/proj/README.md"));
        assert_eq!(hit, Some(outer));
        // An unrelated path attributes to nothing.
        assert!(
            watcher
                .session_for_path(std::path::Path::new("/elsewhere/x"))
                .is_none()
        );
    }

    #[test]
    fn synthesises_file_changed_event() {
        let state = DaemonState::shared();
        let watcher = FileWatcher::new(state.clone());
        let s = SessionId::new();
        watcher.watch_session(s, PathBuf::from("/tmp/proj"));
        assert!(watcher.record_change(std::path::Path::new("/tmp/proj/src/lib.rs")));
        let events = state.hook_events_for(s);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, HookEvent::FileChanged);
        // An unattributed change records nothing.
        assert!(!watcher.record_change(std::path::Path::new("/nowhere/x")));
        assert_eq!(state.recent_hook_events().len(), 1);
    }
}
