# Overseer

The Overseer is an optional daemon component that monitors and manages running
Claude Code sessions. It intercepts hook events from every session the daemon
controls, applies policy, and acts — blocking a dangerous command, auto-answering
a routine question, enforcing a budget cap — without any involvement from the
session itself.

## Why Daemon-Based Oversight

Sessions run in tmux and are largely unattended. The daemon already intercepts
all hook events (PreToolUse, PostToolUse) via the hook relay, so adding
supervision is a matter of connecting a decision component to that event stream
rather than building a separate channel.

Centralizing oversight in the daemon gives consistent policy across all sessions.
A rule change in `overseer.toml` applies to every running session immediately
without restarting anything. All decisions are logged to
`~/.trusty-mpm/logs/overseer/` as append-only JSONL files, so every action the
overseer takes is reviewable and reversible in the sense that the full audit trail
is always available.

## Design: Strategy Pattern

The daemon holds a single `Box<dyn Overseer>`. Two implementations exist:

```
┌─────────────────────────────────┐
│          Box<dyn Overseer>      │  ← daemon holds this, doesn't care which
└─────────────────────────────────┘
         ↑               ↑
DeterministicOverseer   LlmOverseer
(rule-based)            (wraps deterministic + LLM layer)
```

`DeterministicOverseer` is always available — it requires no API keys, has zero
external dependencies, and adds sub-millisecond latency. `LlmOverseer` wraps
`DeterministicOverseer` entirely: deterministic rules run first as a safety
floor, and the LLM layer only engages for events that are ambiguous after
deterministic evaluation.

No other part of the codebase branches on which implementation is active.

## Event Types

```
PreToolUse      → gate: can block, modify, or approve before CC executes the tool
PostToolUse     → monitor: token optimization, logging, anomaly detection
SessionQuestion → respond: auto-response from pattern table, or LLM if configured
AuthChallenge   → inject known credentials, or surface to human via TUI
BudgetWarning   → warn the session or halt it
```

Events arrive from the hook relay. The overseer's response is either a decision
record (for the audit log and TUI queue) or an action (send-keys into the tmux
session, halt command, credential injection).

## Deterministic Tools

These run whenever the overseer is enabled, regardless of whether an LLM is
configured.

| Tool | Description |
|---|---|
| PreToolUse gate | Blocklist/allowlist pattern matching. Instantly blocks e.g. `rm -rf /`, `git push --force main`, `DROP TABLE`. |
| Auto-responder | Pattern-matched replies to common session questions. Rules are configured in `overseer.toml`. |
| Budget enforcer | Stops or warns sessions that exceed token or cost limits. |
| Credential injector | Injects known credentials when an auth challenge is detected. |
| Rate limiter | Prevents runaway tool use — configurable cap of N tool calls per minute. |
| Circuit breaker | Halts a session on repeated errors or anomalous patterns before damage compounds. |

## LLM Layer

When an LLM provider is configured (any provider via API key), it operates above
the deterministic layer. The decision tree:

```
hook event arrives
    → deterministic gate runs (always)
        blocked   → reject immediately, no LLM call
        approved  → proceed immediately, no LLM call
        ambiguous → escalate to LLM
                        → LLM reviews with session context
                        → approves / rejects / responds / flags for human
```

The LLM is never called for events the deterministic layer can handle
definitively. This keeps latency low and cost predictable. Operators running in
air-gapped or cost-constrained environments get full oversight through the
deterministic layer alone.

## Session Communication

When the overseer responds to a session — whether via auto-responder or LLM —
it writes the response using send-keys into the session's tmux window. The CC
instance reads it as normal stdin. No polling, no special client-side setup, no
protocol changes in the CC instance. The communication channel is the tmux
session itself.

## Configuration

Config lives at `~/.trusty-mpm/framework/hooks/overseer.toml`, installed by
`trusty-mpm install`. The daemon's file watcher picks up changes without restart.
Reset to defaults at any time with `trusty-mpm install --force`.

This is **framework work** (config, not Rust code). The policy file defines the
rules; the Rust code enforces whatever the policy file says.

```toml
[overseer]
enabled = true

[deterministic]
blocklist = ["rm -rf", "git push --force main", "DROP TABLE"]
auto_approve = ["git status", "cargo check", "ls"]
max_tool_calls_per_minute = 60
token_budget_limit = 100000

[auto_responses]
"shall I proceed" = "yes, proceed"
"should I commit" = "yes, commit with an appropriate message"

[llm]
# Optional — omit for deterministic-only mode
# provider = "anthropic"
# model = "claude-haiku-4-5"
# api_key_env = "ANTHROPIC_API_KEY"  # pragma: allowlist secret
```

## Audit Log

All overseer decisions are written to
`~/.trusty-mpm/logs/overseer/YYYY-MM-DD.jsonl`. The file is append-only and
never modified after a record is written.

```json
{"ts":"2026-05-16T18:30:00Z","session":"tmpm-quiet-falcon","event":"PreToolUse","tool":"Bash","input":"rm -rf /tmp/old","decision":"blocked","reason":"blocklist match","handler":"deterministic"}
{"ts":"2026-05-16T18:30:01Z","session":"tmpm-quiet-falcon","event":"SessionQuestion","question":"shall I proceed?","decision":"responded","response":"yes, proceed","handler":"auto_responder"}
```

Fields common to every record: `ts`, `session`, `event`, `decision`, `handler`.
Event-specific fields (tool name, question text, response) are included where
applicable.

## TUI Integration

The overseer feeds an oversight queue in the TUI. Three categories appear there:

- **Flagged** — events the overseer could not resolve automatically and that
  require human review. Displayed with approve/reject actions.
- **Auto-handled** — events the overseer resolved without human input, shown for
  audit.
- **Session health** — rate limit proximity, budget consumption, circuit breaker
  state.

Flagged items block the pending action until the operator responds. Auto-handled
items are informational and dismiss automatically after a configurable interval.

## Project Work vs. Framework Work

**Project work (Rust):**
- The `Overseer` trait definition
- `DeterministicOverseer` implementation (all deterministic tools)
- `LlmOverseer` implementation (wraps deterministic, adds LLM escalation)
- Hook relay wiring — connecting the event stream to the active overseer
- Audit logger (append-only JSONL writer)
- TUI queue feed (oversight queue data model and update path)

**Framework work (config):**
- `overseer.toml` — the rules, patterns, budgets, LLM provider config
- Default policy bundled with `trusty-mpm install`
- Per-project policy overrides (future)

The distinction matters because framework work can be changed by operators
without a rebuild. Project work defines what the framework work can express.

## Related Documents

- [Agent Inheritance](./agent-inheritance.md) — how session agents are composed
  at start time; the overseer is invoked after a session is running
- [Delegation Authority](./delegation-authority.md) — routing table for agent
  delegation; oversight is orthogonal to delegation but both are daemon-managed
- [Instruction Pipeline](./instruction-pipeline.md) — the build steps that
  produce a session's effective instructions; the overseer watches the session
  produced by that pipeline
