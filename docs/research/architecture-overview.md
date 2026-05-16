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
       │   JSON / HTTP IPC (Envelope<Request|Response>)    │
       └───────────────┬──────────────────────────────────┘
                       ▼
            ┌────────────────────────┐
            │   trusty-mpmd daemon   │
            │  ┌──────────────────┐  │
            │  │ Artifact store   │  │  agents / skills / hooks (OOB)
            │  ├──────────────────┤  │
            │  │ Session manager  │  │  tmux / PTY / SDK control models
            │  ├──────────────────┤  │
            │  │ Hook interceptor │  │  PreToolUse/PostToolUse/Stop/...
            │  ├──────────────────┤  │
            │  │ Circuit breaker  │  │  context + delegation limits
            │  ├──────────────────┤  │
            │  │ trusty clients   │  │  memory:3038, search:7878
            │  └──────────────────┘  │
            └───────────┬────────────┘
                        ▼
            ┌────────────────────────┐
            │ Claude Code subprocess │  (one per session)
            └────────────────────────┘
```

All crates share `trusty-mpm-core`, which owns the artifact model, session
types, and the versioned IPC `Envelope`.

## Workspace crates

| Crate | Binary | Responsibility |
|-------|--------|----------------|
| `trusty-mpm-core` | (lib) | Artifact model, session types, IPC protocol, errors |
| `trusty-mpm-daemon` | `trusty-mpmd` | Resident process: sessions, hooks, artifact store, trusty clients |
| `trusty-mpm-cli` | `trusty-mpm` | Thin client; talks to the daemon over IPC |
| `trusty-mpm-tui` | `trusty-mpm-tui` | ratatui dashboard |
| `trusty-mpm-telegram` | `trusty-mpm-telegram` | Telegram remote-management bot |

## How it replaces claude-mpm

| claude-mpm | trusty-mpm |
|------------|------------|
| Python CLI (`claude-mpm`) | Rust CLI (`trusty-mpm`) + daemon |
| Hook = spawn `python3 -m ...` | Hook = in-daemon function call |
| Hook wiring in `.claude/settings.json` | Daemon intercepts events directly (OOB) |
| Agents in `.claude/agents/*.md` | Same files, served from daemon artifact store |
| Skills as bundled `.md` | Same format, resolved by daemon before Claude Code sees prompt |
| MCP clients via subprocess | Native Rust clients to trusty-memory / trusty-search |
| FastAPI service for UI | axum HTTP API + ratatui TUI + Telegram |

## Process lifecycle

1. `trusty-mpmd` starts, loads artifacts, connects to trusty-memory/search.
2. `trusty-mpm start` sends `Request::StartSession`.
3. Daemon launches Claude Code under the configured control model
   (tmux by default — see `session-control-models.md`).
4. Hook events route to the daemon; the daemon applies circuit-breaker logic,
   skill resolution, and model-tier enforcement in-process.
5. TUI and Telegram subscribe to the same daemon API for observability and
   remote control.

## Design principles

- **Deterministic management** — start/stop/status/approval need no LLM.
- **Optional summarization** — LLM is used only for human-friendly status
  reports (the "AI Commander" pattern, see `dashboard-telegram.md`).
- **Artifact compatibility** — claude-mpm agent/skill/hook formats are a hard
  compatibility contract (see `artifact-compatibility.md`).
- **Stable toolchain** — `rust-toolchain.toml` pins `stable`; edition 2021 to
  match sibling trusty crates.
