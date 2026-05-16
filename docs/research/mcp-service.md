# MCP Service — Claude Code ↔ trusty-mpm

## Purpose

trusty-mpm exposes an **MCP server** so that Claude Code sessions (and their
subagents) can talk to the daemon directly — to enumerate sibling sessions,
request agent delegations, protect their own context window, and inspect
circuit-breaker state. MCP is the protocol Claude Code already speaks, so
trusty-mpm exposes an MCP server rather than inventing a bespoke channel.

The implementation lives in the `trusty-mpm-mcp` crate; the daemon (`trusty-mpmd`)
runs it as a subprocess MCP server over stdio (`trusty-mpmd mcp`).

## Why an MCP server (not just the HTTP API)

The daemon already has an HTTP API (used by the CLI, TUI, and Telegram bot). MCP
is added on top because:

- **Claude Code launches MCP servers natively.** A session lists `trusty-mpm` in
  its `.mcp.json` and the orchestration tools appear in the model's tool list —
  no client code, no prompt engineering.
- **Subagents inherit it.** Any subagent delegated by the PM can call the same
  tools, so a research agent can itself ask the daemon for circuit-breaker state.
- **It is the symmetry that makes trusty-mpm self-hosting.** Claude Code drives
  the daemon; the daemon drives Claude Code (via tmux). MCP closes that loop.

## Architecture

```
┌────────────────────────┐         ┌───────────────────────────┐
│ Claude Code session    │  stdio  │ trusty-mpmd mcp           │
│  (lists trusty-mpm in  │ JSON-RPC│  ┌──────────────────────┐ │
│   .mcp.json)           │◄───────►│  │ trusty-mpm-mcp       │ │
│                        │  2.0    │  │  dispatch()          │ │
└────────────────────────┘         │  └──────────┬───────────┘ │
                                    │             ▼             │
                                    │  ┌──────────────────────┐ │
                                    │  │ StateBackend         │ │
                                    │  │ (impl Orchestrator-  │ │
                                    │  │  Backend)            │ │
                                    │  └──────────┬───────────┘ │
                                    │             ▼             │
                                    │       DaemonState         │
                                    └───────────────────────────┘
```

The transport and JSON-RPC envelopes are reused from `trusty-mcp-core` (the
shared trusty-common crate). `trusty-mpm-mcp` only adds the tool catalog and the
`dispatch` routing; the daemon supplies behaviour through the
`OrchestratorBackend` trait.

## Design: Dependency Inversion seam

`trusty-mpm-mcp` is deliberately ignorant of daemon internals (process spawning,
tmux, sockets). It defines a trait:

```rust
#[async_trait]
pub trait OrchestratorBackend: Send + Sync {
    async fn session_list(&self) -> Result<Value, String>;
    async fn session_status(&self, session_id: &str) -> Result<Value, String>;
    async fn agent_delegate(&self, session_id: &str, agent: &str,
                            task: &str, tier: Option<&str>) -> Result<Value, String>;
    async fn memory_protect(&self, session_id: &str,
                            used_tokens: u64, window_tokens: u64) -> Result<Value, String>;
    async fn circuit_breaker_status(&self, agent: Option<&str>) -> Result<Value, String>;
    async fn hook_event(&self, session_id: &str, event: &str,
                        payload: Value) -> Result<Value, String>;
}
```

The daemon's `StateBackend` (in `trusty-mpm-daemon/src/mcp_backend.rs`)
implements it over the shared `DaemonState`. The MCP crate is unit-tested with an
in-memory `MockBackend`; the daemon's backend is tested against a real
`DaemonState`. Neither needs the other to be tested.

## Tool definitions

All six tools are advertised through `tools/list` with a JSON Schema
`inputSchema`. They are wrapped in the standard MCP `content` array on the way
back, with `isError` set when the call fails.

| Tool | Arguments | Returns |
|------|-----------|---------|
| `session_list` | — | array of session summaries |
| `session_status` | `session_id` | session + memory + delegation count/tree |
| `agent_delegate` | `session_id`, `agent`, `task`, `tier?` | new delegation id + tier + breaker state |
| `memory_protect` | `session_id`, `used_tokens`, `window_tokens` | usage fraction + pressure level |
| `circuit_breaker_status` | `agent?` | one or all agents' breaker state |
| `hook_event` | `session_id`, `event`, `payload?` | acknowledgement |

### Semantics worth noting

- **`agent_delegate` is gated.** Before recording the delegation the backend
  consults the agent's circuit breaker; an open breaker refuses the delegation
  with an explanatory `isError` result rather than silently queueing it.
- **`memory_protect` classifies pressure.** It stores the token snapshot and
  returns `ok` / `warn` / `alert` / `compact` per the daemon's `MemoryConfig`
  (see `trusty-integration.md` and `session-control-models.md`).
- **`hook_event` drives the circuit breaker.** A `SubagentStopFailure` event for
  an agent counts as a failure; a plain `SubagentStop` counts as a success. This
  lets sessions feed the same observability pipeline the HTTP relay feeds.

## JSON-RPC method surface

`dispatch` handles the full MCP handshake:

- `initialize` → standard `protocolVersion` / `capabilities.tools` / `serverInfo`
  (built by `trusty_mcp_core::initialize_response`).
- `tools/list` → the six-tool catalog.
- `tools/call` → routes by `name`, parses `arguments`, calls the backend.
- `ping` → empty `{}`.
- Notifications (no `id`) are suppressed (no reply written).
- Unknown methods → JSON-RPC `METHOD_NOT_FOUND`.

## Running it

```sh
trusty-mpmd mcp        # MCP server over stdio (tracing goes to stderr)
trusty-mpmd http       # the resident HTTP daemon (default mode)
```

A Claude Code project registers it in `.mcp.json`:

```json
{
  "mcpServers": {
    "trusty-mpm": { "command": "trusty-mpmd", "args": ["mcp"] }
  }
}
```

## Testing

`cargo test -p trusty-mpm-mcp` covers the tool catalog shape, argument parsing,
the handshake, every tool call, and the error paths against `MockBackend`.
`cargo test -p trusty-mpm-daemon` covers `StateBackend` against a real
`DaemonState`, including the breaker-gating and pressure-classification paths.

## Status

Implemented: `trusty-mpm-mcp` crate (tool catalog + `dispatch` + trait),
`StateBackend` in the daemon, and the `trusty-mpmd mcp` run mode.
Pending: streaming tool results and an MCP `resources` surface for artifact
serving — tracked in the GitHub issues.
