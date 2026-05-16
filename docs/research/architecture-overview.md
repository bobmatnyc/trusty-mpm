# trusty-mpm Architecture Overview

## Purpose

`trusty-mpm` is a Rust reimagining of [`claude-mpm`](https://github.com/bobmatnyc/claude-mpm),
the Claude Multi-Agent Project Manager. It keeps claude-mpm's orchestration model
(PM → specialist subagents, skills, hooks, MCP integration) but replaces the
per-invocation Python process model with a single stable daemon.

## Why a daemon

claude-mpm runs as a Python CLI. Every Claude Code hook event
(`PreToolUse`, `PostToolUse`, `Stop`, ...) spawns a fresh `python3 -m
claude_mpm.hooks.*` process. For a busy session this means:

- **Startup tax** — interpreter boot + imports on every tool call (~100-400ms).
- **No shared state** — circuit-breaker counters, session memory, and MCP
  client connections must be re-loaded or persisted to disk each time.
- **Memory churn** — many short-lived processes instead of one resident image.

A long-running daemon (`trusty-mpmd`) fixes all three: hooks become in-process
function calls, state lives in memory, and MCP clients stay connected.

## Component map

```
┌──────────────┐   ┌──────────────┐   ┌───────────────────┐
│ trusty-mpm   │   │ trusty-mpm-  │   │ trusty-mpm-       │
│ (CLI client) │   │ tui          │   │ telegram (bot)    │
└──────┬───────┘   └──────┬───────┘   └─────────┬─────────┘
       │   JSON / HTTP API (sessions, events, breakers)    │
       └───────────────┬──────────────────────────────────┘
                       ▼
            ┌────────────────────────────────┐
            │   trusty-mpmd daemon           │
            │  ┌──────────────────────────┐  │
            │  │ DaemonState (shared)     │  │  sessions / delegations /
            │  │                          │  │  breakers / memory / hooks
            │  ├──────────────────────────┤  │
            │  │ HTTP API + hook relay    │  │  POST /hooks  (all 32 events)
            │  ├──────────────────────────┤  │
            │  │ MCP server (stdio)       │  │  6 orchestration tools
            │  ├──────────────────────────┤  │
            │  │ tmux session manager     │  │  named sessions, send-keys
            │  ├──────────────────────────┤  │
            │  │ file watcher             │  │  multi-session file monitor
            │  ├──────────────────────────┤  │
            │  │ circuit breaker          │  │  per-agent failure / depth caps
            │  ├──────────────────────────┤  │
            │  │ trusty clients           │  │  memory + search
            │  └──────────────────────────┘  │
            └───────────┬────────────────────┘
                        │ tmux send-keys / capture-pane
                        ▼
            ┌────────────────────────┐
            │ Claude Code subprocess │  (one per session, inside tmux)
            │  ─ lists trusty-mpm in │
            │    its .mcp.json ──────┼──► MCP tools call back into the daemon
            │  ─ hook events ────────┼──► POST /hooks relay
            └────────────────────────┘
```

All crates share `trusty-mpm-core`, which owns the artifact model, session and
agent types, the hook vocabulary, the memory and circuit-breaker models, the
tmux command builder, and the versioned IPC `Envelope`.

## Workspace crates

| Crate | Binary | Responsibility |
|-------|--------|----------------|
| `trusty-mpm-core` | (lib) | Artifacts, sessions, agents, hooks, memory, circuit, tmux types, IPC |
| `trusty-mpm-mcp` | (lib) | MCP server: 6 orchestration tools, `OrchestratorBackend` trait, `dispatch` |
| `trusty-mpm-daemon` | `trusty-mpmd` | Resident daemon: state, HTTP API, hook relay, MCP backend, tmux, watcher |
| `trusty-mpm-cli` | `trusty-mpm` | Thin client; talks to the daemon over HTTP |
| `trusty-mpm-tui` | `trusty-mpm-tui` | ratatui multi-session dashboard |
| `trusty-mpm-telegram` | `trusty-mpm-telegram` | Telegram remote-management bot + alerts |

## Shared crates from trusty-common

trusty-mpm depends on the sibling `trusty-common` workspace by path:

- **`trusty-mcp-core`** — JSON-RPC 2.0 / MCP envelopes and the stdio runner.
  `trusty-mpm-mcp` builds its tool layer on top; the daemon's `trusty-mpmd mcp`
  mode runs `trusty_mcp_core::run_stdio_loop`.
- **`trusty-common`** — shared utilities and provider-agnostic chat (used for
  the optional LLM status summaries).

See `trusty-integration.md` for the full integration surface.

## The four daemon faces

trusty-mpm exposes four ways to reach the same `DaemonState`:

1. **HTTP API** — for the CLI, TUI, and Telegram bot (`GET /sessions`,
   `GET /events`, `GET /breakers`, `POST /sessions`, `DELETE /sessions/:id`).
2. **Universal hook relay** — `POST /hooks` ingests *every* Claude Code hook
   event (all 32) from a tiny forwarder shim. See `session-control-models.md`.
3. **MCP server** — `trusty-mpmd mcp` over stdio gives Claude Code sessions the
   six orchestration tools. See `mcp-service.md`.
4. **tmux session control** — the daemon launches and drives Claude Code inside
   named tmux sessions. See `session-control-models.md`.

## How it replaces claude-mpm

| claude-mpm | trusty-mpm |
|------------|------------|
| Python CLI (`claude-mpm`) | Rust CLI (`trusty-mpm`) + daemon |
| Hook = spawn `python3 -m ...` | Hook = `POST /hooks` to a resident daemon |
| 7 wired hook events | All 32 events relayed for full observability |
| Agents in `.claude/agents/*.md` | Same files, served from daemon artifact store |
| Skills as bundled `.md` | Same format, resolved by the daemon |
| MCP clients via subprocess | Native Rust clients + an MCP *server* for sessions |
| Single-session dashboard | Multi-session ratatui dashboard |
| FastAPI service for UI | axum HTTP API + ratatui TUI + Telegram |

## Process lifecycle

1. `trusty-mpmd http` starts, builds shared `DaemonState`, loads artifacts,
   connects to trusty-memory/search.
2. `trusty-mpm start` (or the Telegram bot) registers a session.
3. The daemon launches Claude Code under the configured control model
   (tmux by default — see `session-control-models.md`).
4. Hook events route to `POST /hooks`; the daemon applies circuit-breaker
   logic, memory-pressure classification, and model-tier enforcement in-process.
5. The Claude Code session lists `trusty-mpm` in its `.mcp.json`; it and its
   subagents call the MCP orchestration tools.
6. TUI and Telegram subscribe to the daemon API for observability and control.

## Design principles

- **Deterministic management** — start/stop/status/approval need no LLM.
- **Optional summarization** — LLM is used only for human-friendly status
  reports (see `dashboard-telegram.md`).
- **Dependency Inversion at the protocol seams** — `trusty-mpm-mcp` defines an
  `OrchestratorBackend` trait; the daemon supplies the implementation. Either
  side is testable without the other.
- **Pure logic, isolated I/O** — the tmux command builder, alert formatting,
  command parsing, and circuit-breaker math are pure and unit-tested; process
  spawning and sockets live in thin daemon-only wrappers.
- **Artifact compatibility** — claude-mpm agent/skill/hook formats are a hard
  compatibility contract (see `artifact-compatibility.md`).
- **Stable toolchain** — `rust-toolchain.toml` pins `stable`; edition 2024 to
  match the sibling trusty-common crates.
