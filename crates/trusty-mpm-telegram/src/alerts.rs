//! Telegram alert formatting and event-subscription filtering.
//!
//! Why: the Telegram bot pushes alerts for memory pressure and selected hook
//! events. Keeping the *decision* of what to alert on and *how to format* it
//! pure (no network) makes it unit-testable independent of teloxide and the
//! daemon. The bot's runtime just calls these functions.
//! What: [`AlertConfig`] (which event categories the operator subscribed to),
//! [`should_alert`] (the subscription filter), and [`format_memory_alert`] /
//! [`format_event_alert`] (the human-readable message bodies).
//! Test: `cargo test -p trusty-mpm-telegram` covers the filter and formatting.

// The teloxide alert-pusher loop that calls these lands in a follow-up issue;
// until then the alert logic is exercised only by its own tests.
#![allow(dead_code)]

use trusty_mpm_core::hook::{HookCategory, HookEvent};
use trusty_mpm_core::memory::MemoryPressure;

/// Which hook-event categories an operator wants Telegram alerts for.
///
/// Why: 32 hook events firing on every tool call would spam the chat; the
/// operator opts in to categories (e.g. just permission + memory).
/// What: a set of subscribed [`HookCategory`] values, plus a memory toggle.
/// Test: `subscription_filter_respects_categories`.
#[derive(Debug, Clone, Default)]
pub struct AlertConfig {
    /// Hook categories the operator subscribed to.
    pub categories: Vec<HookCategory>,
    /// When true, memory-pressure alerts are pushed.
    pub memory_alerts: bool,
}

impl AlertConfig {
    /// A sensible default: alert on permission and agent events plus memory.
    ///
    /// Why: these are the categories an absent operator most needs to see —
    /// a session blocked on a permission prompt, an agent failing, or a
    /// session about to hit its context limit.
    /// What: subscribes `Permission` + `Agent` categories and memory alerts.
    /// Test: `default_config_alerts_on_permission`.
    pub fn recommended() -> Self {
        Self {
            categories: vec![HookCategory::Permission, HookCategory::Agent],
            memory_alerts: true,
        }
    }
}

/// True if a hook event should produce a Telegram alert under `config`.
///
/// Why: the bot consults this for every event the daemon reports.
/// What: checks the event's category against the subscribed set.
/// Test: `subscription_filter_respects_categories`.
pub fn should_alert(config: &AlertConfig, event: HookEvent) -> bool {
    config.categories.contains(&event.category())
}

/// True if a memory-pressure level warrants a Telegram alert.
///
/// Why: only `Alert` and `Compact` levels are worth interrupting the operator;
/// `Warn` is shown on the dashboard but not pushed.
/// What: returns true for `Alert`/`Compact` when memory alerts are enabled.
/// Test: `memory_alert_threshold`.
pub fn should_memory_alert(config: &AlertConfig, pressure: MemoryPressure) -> bool {
    config.memory_alerts && pressure >= MemoryPressure::Alert
}

/// Format a memory-pressure alert message.
///
/// Why: the operator needs a glanceable message naming the session and level.
/// What: a one-line string with the session id and pressure level.
/// Test: `memory_alert_message_names_session`.
pub fn format_memory_alert(session_id: &str, pressure: MemoryPressure, fraction: f32) -> String {
    let pct = (fraction * 100.0).round() as u32;
    format!(
        "⚠️ trusty-mpm: session {session_id} memory pressure {pressure:?} ({pct}% of context window)"
    )
}

/// Format a hook-event alert message.
///
/// Why: a uniform, short message for any subscribed event.
/// What: names the event and the originating session.
/// Test: `event_alert_message_names_event`.
pub fn format_event_alert(session_id: &str, event: HookEvent) -> String {
    format!(
        "🔔 trusty-mpm: {} in session {session_id}",
        event.wire_name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_alerts_on_permission() {
        let cfg = AlertConfig::recommended();
        assert!(should_alert(&cfg, HookEvent::PermissionDenied));
        assert!(cfg.memory_alerts);
    }

    #[test]
    fn subscription_filter_respects_categories() {
        let cfg = AlertConfig {
            categories: vec![HookCategory::Permission],
            memory_alerts: false,
        };
        // Subscribed category fires.
        assert!(should_alert(&cfg, HookEvent::PermissionGranted));
        // Unsubscribed category (Tool) does not.
        assert!(!should_alert(&cfg, HookEvent::PreToolUse));
    }

    #[test]
    fn memory_alert_threshold() {
        let cfg = AlertConfig {
            categories: vec![],
            memory_alerts: true,
        };
        assert!(!should_memory_alert(&cfg, MemoryPressure::Warn));
        assert!(should_memory_alert(&cfg, MemoryPressure::Alert));
        assert!(should_memory_alert(&cfg, MemoryPressure::Compact));
        // Disabled config never alerts.
        let off = AlertConfig {
            categories: vec![],
            memory_alerts: false,
        };
        assert!(!should_memory_alert(&off, MemoryPressure::Compact));
    }

    #[test]
    fn memory_alert_message_names_session() {
        let msg = format_memory_alert("sess-1", MemoryPressure::Alert, 0.86);
        assert!(msg.contains("sess-1"));
        assert!(msg.contains("86%"));
        assert!(msg.contains("Alert"));
    }

    #[test]
    fn event_alert_message_names_event() {
        let msg = format_event_alert("sess-2", HookEvent::SubagentStopFailure);
        assert!(msg.contains("sess-2"));
        assert!(msg.contains("SubagentStopFailure"));
    }
}
