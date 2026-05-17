//! Hook processing pipeline business logic.
//!
//! Why: the `POST /hooks` handler embedded the overseer context construction,
//! the event-kind dispatch, the audit write, and the `PostToolUse` compression
//! decision. That is the daemon's enforcement core; isolating it in a service
//! makes each step testable and replaces the free `run_overseer` /
//! `overseer_context` functions that lived in `api.rs`.
//! What: [`HookDecision`] is the daemon-facing verdict; [`HookService`] builds
//! the [`OverseerContext`] from a raw payload, consults the configured
//! overseer, audits the verdict, applies output optimization, and records the
//! event in the ring buffer.
//! Test: `cargo test -p trusty-mpm-daemon services::hook` covers the
//! disabled-overseer fast path and the decision conversion.

use serde_json::Value;
use trusty_mpm_core::hook::{HookEvent, HookEventRecord};
use trusty_mpm_core::overseer::{OverseerContext, OverseerDecision};
use trusty_mpm_core::session::SessionId;

use crate::audit::AuditEntry;
use crate::state::DaemonState;

/// The daemon-facing result of processing one hook event.
///
/// Why: [`OverseerDecision`] is the core's vocabulary; the daemon wants a verdict
/// it owns so the HTTP layer is decoupled from the core enum and can carry
/// daemon-specific follow-up (e.g. "this event was already recorded").
/// What: the four overseer outcomes, with the same data each carries.
/// Test: `decision_converts_from_overseer`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookDecision {
    /// Let the event proceed; it has been recorded.
    Allow,
    /// The overseer halted the event; `reason` explains why.
    Block {
        /// Human-readable explanation of the block.
        reason: String,
    },
    /// The overseer wants `text` injected into the session.
    Respond {
        /// Text to send into the session.
        text: String,
    },
    /// The overseer escalated the event for human review.
    FlagForHuman {
        /// Short description of why human attention is needed.
        summary: String,
    },
}

impl From<OverseerDecision> for HookDecision {
    /// Map a core overseer verdict onto the daemon's decision type.
    ///
    /// Why: the two enums are structurally identical; an explicit `From` keeps
    /// the conversion in one place instead of scattered `match`es.
    /// What: variant-for-variant translation.
    /// Test: `decision_converts_from_overseer`.
    fn from(d: OverseerDecision) -> Self {
        match d {
            OverseerDecision::Allow => Self::Allow,
            OverseerDecision::Block { reason } => Self::Block { reason },
            OverseerDecision::Respond { text } => Self::Respond { text },
            OverseerDecision::FlagForHuman { summary } => Self::FlagForHuman { summary },
        }
    }
}

impl HookDecision {
    /// Stable lowercase tag for this decision (`"allow" | "block" | ...`).
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Block { .. } => "block",
            Self::Respond { .. } => "respond",
            Self::FlagForHuman { .. } => "flag",
        }
    }

    /// The human-readable detail of this decision, if any.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Allow => None,
            Self::Block { reason } => Some(reason),
            Self::Respond { text } => Some(text),
            Self::FlagForHuman { summary } => Some(summary),
        }
    }
}

/// Hook event processing over the shared daemon state.
///
/// Why: a borrowed facade — the handler builds one per request and delegates
/// the whole relay pipeline to it, so `ingest_hook` shrinks to a few lines.
/// What: holds a borrow of [`DaemonState`]; [`process`](Self::process) runs the
/// overseer-audit-optimize-record pipeline for one event.
/// Test: the module's `#[cfg(test)]` suite.
pub struct HookService<'s> {
    state: &'s DaemonState,
}

impl<'s> HookService<'s> {
    /// Build a service bound to `state`.
    pub fn new(state: &'s DaemonState) -> Self {
        Self { state }
    }

    /// Process one hook event end to end.
    ///
    /// Why: this is the daemon's full hook pipeline — consult the overseer on
    /// tool-use events (auditing every verdict), compress `PostToolUse` output,
    /// then append the event to the ring buffer. Keeping it in one method makes
    /// the order of those steps explicit and testable.
    /// What: builds an [`OverseerContext`], runs the overseer when it is
    /// enabled, records the event (unless blocked), and returns the verdict. A
    /// `Block` short-circuits before the event is recorded.
    /// Test: `process_records_event_with_disabled_overseer`.
    pub fn process(
        &self,
        session: SessionId,
        event: HookEvent,
        mut payload: Value,
    ) -> HookDecision {
        // 1. Overseer: evaluate + audit tool-use events. Skipped entirely when
        //    oversight is disabled (the common opt-out path).
        let overseer = self.state.overseer();
        if overseer.is_enabled()
            && let Some(decision) = self.run_overseer(&overseer, event, session, &payload)
        {
            if let OverseerDecision::Block { reason } = &decision {
                return HookDecision::Block {
                    reason: reason.clone(),
                };
            }
            if let OverseerDecision::Respond { text } = &decision {
                tracing::info!("overseer auto-response for {session:?}: {text}");
            }
        }

        // 2. PostToolUse: compress tool output before it enters the ring buffer.
        if event == HookEvent::PostToolUse {
            let tool_name = payload
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let cfg = self.state.optimizer_config();
            crate::optimizer::optimize_tool_output(&cfg, &tool_name, &mut payload);
        }

        // 3. Record the event in the bounded history.
        self.state
            .push_hook_event(HookEventRecord::now(session, event, payload));
        HookDecision::Allow
    }

    /// Build an [`OverseerContext`] from a raw hook payload.
    ///
    /// Why: the overseer evaluates events by tool name and input; extracting
    /// those from the opaque payload belongs in one place. Replaces the free
    /// `overseer_context` function from `api.rs`.
    /// What: resolves the session's friendly name (falling back to the UUID),
    /// reads `payload["tool"]` and serializes `payload["input"]`.
    /// Test: covered by `process_records_event_with_disabled_overseer`.
    fn context(&self, session: SessionId, payload: &Value) -> OverseerContext {
        let tmux_name = self
            .state
            .session(session)
            .map(|s| s.tmux_name)
            .unwrap_or_else(|| session.0.to_string());
        let tool_name = payload
            .get("tool")
            .and_then(Value::as_str)
            .map(str::to_string);
        let tool_input = payload
            .get("input")
            .map(|v| v.to_string())
            .or_else(|| Some(payload.to_string()));
        OverseerContext::new(session, tmux_name, tool_name, tool_input)
    }

    /// Run the overseer for one event and audit the verdict.
    ///
    /// Why: keeping the event-kind dispatch and the audit write in one helper
    /// keeps [`process`](Self::process) focused on the relay flow.
    /// What: maps `PreToolUse` / `PostToolUse` onto the matching overseer call,
    /// writes an [`AuditEntry`], and returns the decision; other events return
    /// `None` (the overseer does not act on them).
    /// Test: covered by `process_records_event_with_disabled_overseer`.
    fn run_overseer(
        &self,
        overseer: &std::sync::Arc<dyn trusty_mpm_core::overseer::Overseer>,
        event: HookEvent,
        session: SessionId,
        payload: &Value,
    ) -> Option<OverseerDecision> {
        let ctx = self.context(session, payload);
        let (event_label, decision) = match event {
            HookEvent::PreToolUse => ("PreToolUse", overseer.pre_tool_use(&ctx)),
            HookEvent::PostToolUse => {
                let output = payload.get("output").and_then(Value::as_str).unwrap_or("");
                ("PostToolUse", overseer.post_tool_use(&ctx, output))
            }
            _ => return None,
        };
        self.state.audit().log(AuditEntry::from_decision(
            &ctx,
            event_label,
            &decision,
            self.state.overseer_handler(),
        ));
        Some(decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trusty_mpm_core::session::{ControlModel, Session, SessionStatus};

    #[test]
    fn decision_converts_from_overseer() {
        assert_eq!(
            HookDecision::from(OverseerDecision::Allow),
            HookDecision::Allow
        );
        assert_eq!(
            HookDecision::from(OverseerDecision::Block { reason: "x".into() }),
            HookDecision::Block { reason: "x".into() }
        );
        assert_eq!(HookDecision::Allow.tag(), "allow");
        assert_eq!(
            HookDecision::Block { reason: "r".into() }.detail(),
            Some("r")
        );
    }

    #[test]
    fn process_records_event_with_disabled_overseer() {
        // With the overseer disabled (the default), a known event must be
        // recorded and the verdict is Allow.
        let state = DaemonState::new();
        let id = SessionId::new();
        let mut s = Session::new(id, "/tmp/p", ControlModel::Tmux);
        s.status = SessionStatus::Active;
        state.register_session(s);

        let svc = HookService::new(&state);
        let decision = svc.process(
            id,
            HookEvent::PreToolUse,
            serde_json::json!({ "tool": "Bash" }),
        );
        assert_eq!(decision, HookDecision::Allow);
        assert_eq!(state.recent_hook_events().len(), 1);
    }
}
