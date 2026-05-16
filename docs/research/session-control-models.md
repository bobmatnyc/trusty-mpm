# Session Control Models

trusty-mpm hosts each Claude Code session under one of three control models.
**tmux is the primary model**; PTY and SDK are fallbacks.

## 1. tmux (primary)

A named tmux session per Claude Code session. The daemon creates the session
detached, sends keystrokes to launch and drive Claude Code, and captures pane
output for the dashboard.

### Why tmux first

- **Survivable** — the session outlives the daemon; a daemon restart re-attaches.
- **Inspectable** — an operator can `tmux attach` to any session directly.
- **Battle-tested** — `tmux send-keys` / `capture-pane` is the same mechanism
  `ai-commander` and `open-mpm` already use to drive agent CLIs.

### Patterns adopted from ai-commander and open-mpm

The design is distilled from `ai-commander`'s `commander-tmux` crate (its
`TmuxOrchestrator`) and `open-mpm`'s `tm` module:

| Operation | tmux command | trusty-mpm |
|-----------|--------------|------------|
| Create named session | `new-session -d -s <name> [-c <dir>]` | `TmuxCommand::NewSession` |
| Probe a session | `has-session -t <name>` | `TmuxCommand::HasSession` |
| Enumerate sessions | `list-sessions -F <fmt>` | `TmuxCommand::ListSessions` |
| Send a command line | `send-keys -t <t> -l <text>` then `send-keys -t <t> Enter` | `TmuxDriver::send_line` |
| Capture output | `capture-pane -t <t> -p [-S -<n>]` | `TmuxCommand::CapturePane` |
| Destroy a session | `kill-session -t <name>` | `TmuxCommand::KillSession` |

Two lessons carried over verbatim:

1. **Literal text needs `-l`.** `send-keys` interprets bare words like `Enter`
   or `C-c` as key names. Command text must be sent with `-l` (literal); the
   `Enter` to submit it is sent as a *separate* non-literal `send-keys`. This is
   exactly what `commander-tmux`'s `send_line` does, and `TmuxDriver::send_line`
   mirrors it.
2. **"No server running" is an empty list, not an error.** `tmux list-sessions`
   exits non-zero when there are zero sessions; `TmuxDriver::list_sessions`
   classifies that stderr string as `Ok(vec![])` — the same special-case
   `commander-tmux` makes.

### Two-layer split

- **`trusty-mpm-core::tmux`** — pure: `TmuxTarget`, `TmuxCommand`, and
  `tmux_argv()` which renders a command to an argv vector. No process spawning,
  so it is fully unit-testable.
- **`trusty-mpm-daemon::tmux`** — the `TmuxDriver`: resolves the `tmux` binary
  once (`which tmux`), executes the rendered argv, and interprets exit status.
  Tested for binary-discovery degradation and `list-sessions` row parsing
  without needing tmux installed; live operations are `#[ignore]` integration
  tests.

Session names are `trusty-mpm-<session-id>` so the dashboard can correlate a
tmux session with a `SessionId`.

## 2. PTY (fallback)

When tmux is not on `PATH`, the daemon owns a pseudo-terminal directly. The
daemon checks availability at startup (`TmuxDriver::is_available`) and logs
which model is in use. PTY mode loses survivability (the session dies with the
daemon) but needs no external dependency.

## 3. SDK / headless (non-interactive)

For non-interactive runs the daemon drives Claude Code via its SDK / headless
mode — no terminal at all. Used for scripted, one-shot delegations where the
interactive control surface is unnecessary.

## Choosing a model

```
tmux on PATH?
├─ yes → tmux  (default; survivable, inspectable)
└─ no  → interactive run needed?
         ├─ yes → PTY  (daemon-owned pseudo-terminal)
         └─ no  → SDK  (headless, scripted)
```

`ControlModel` (`Tmux` / `Pty` / `Sdk`) is recorded on every `Session` so the
dashboard and the MCP `session_status` tool can report it.

## Memory protection

A session that silently runs into its context limit loses work and produces
degraded output. trusty-mpm tracks token usage per session and acts at
configurable thresholds.

### Thresholds

`MemoryConfig` holds three fractions of the context window, validated to be
strictly ordered and within `(0, 1]`:

| Threshold | Default | Action |
|-----------|---------|--------|
| `warn_at` | 0.70 | Non-blocking warning; dashboard gauge turns amber |
| `alert_at` | 0.85 | Telegram alert pushed to the operator |
| `compact_at` | 0.90 | Daemon triggers an automatic compaction |

`MemoryUsage { used_tokens, window_tokens }` is classified by
`MemoryUsage::pressure(&config)` into `MemoryPressure::{Ok, Warn, Alert,
Compact}` — the single source of truth shared by the daemon, the TUI gauge, and
the Telegram alert.

### How usage arrives

Two paths feed `DaemonState::record_memory`:

1. **`TokenUsageUpdate` hook events** relayed via `POST /hooks`.
2. **The MCP `memory_protect` tool** — a session (or subagent) can proactively
   report its own usage and receive the pressure classification back.

### Acting on pressure

- **Warn** — surfaced only on the dashboard memory gauge.
- **Alert** — the Telegram bot pushes a memory alert (see `dashboard-telegram.md`).
- **Compact** — the daemon snapshots important context into **trusty-memory**
  *before* triggering compaction, so nothing is lost (see `trusty-integration.md`).

### Why centralise the math

Putting the threshold logic in `trusty-mpm-core::memory` means "85%" means the
same thing everywhere — the daemon's compaction trigger, the TUI gauge, and the
Telegram alert filter all call the same `pressure()` function. Boundary
behaviour is unit-tested at exactly `0.70`, `0.85`, and `0.90`.

## Status

Implemented: the `core::tmux` command builder, the daemon's `TmuxDriver`, the
`ControlModel` enum, and the full memory-protection model
(`MemoryConfig`/`MemoryUsage`/`MemoryPressure`).
Pending: the session-start command path that spawns Claude Code into a tmux
session, PTY hosting, and the trusty-memory pre-compaction snapshot — tracked in
the GitHub issues.
