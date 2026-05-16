# Dashboard & Telegram

Two operator interfaces sit on top of the daemon API: a local ratatui TUI and
a remote Telegram bot. Both are pure clients — they hold no orchestration
state and require no LLM for management operations.

## ratatui TUI dashboard

### Goals

At-a-glance visibility into: active sessions, agent delegations, circuit
breaker status, and trusty-memory/search stats.

### Layout

```
┌─ trusty-mpm ───────────────────────────── daemon: ●  127.0.0.1:7880 ┐
│ Sessions                          │ Selected Session               │
│ ▸ a1b2  /proj/foo   active   d:2  │ id: a1b2c3d4                    │
│   c3d4  /proj/bar   detached d:0  │ workdir: /proj/foo              │
│   e5f6  /proj/baz   approval d:1  │ control: tmux                   │
│                                   │ status: active                 │
├───────────────────────────────────┤ delegations:                   │
│ Circuit Breakers                  │  • planner   → opus    running  │
│  context     ████████░░  82%  ok  │  • engineer  → sonnet  running  │
│  delegation  ███░░░░░░░  30%  ok  │                                 │
├───────────────────────────────────┼─────────────────────────────────┤
│ trusty                            │ Event Log                       │
│  memory  :3038  ● 1.2k nodes      │ 12:01 PreToolUse  Bash   allow  │
│  search  :7878  ● 8 indexes       │ 12:01 delegation start planner  │
└───────────────────────────────────┴─────────────────────────────────┘
 q quit  ↑↓ select  a approve  d deny  s stop  r refresh
```

### Widgets

| Widget | Source | Update |
|--------|--------|--------|
| Sessions list | `Request::ListSessions` | poll 1s |
| Session detail | selected session snapshot | on select / poll |
| Circuit breakers | daemon breaker gauges | poll 1s |
| trusty stats | memory/search client health | poll 5s |
| Event log | daemon event stream (SSE) | streamed |

### Implementation

- `ratatui` + `crossterm` backend.
- Single `tokio` task polls the daemon HTTP API; a `mpsc` channel feeds the
  render loop. Keeps rendering off the network path.
- Key handling is deterministic: `a`/`d` send `Request::ResolveApproval`,
  `s` sends `Request::StopSession`. No LLM involved.

## Telegram bot

### Goals

Remote management from a phone: start/stop sessions, view status, and
approve/deny permission requests when a session is blocked.

### Architecture

```
Telegram ⇄ teloxide dispatcher ⇄ daemon HTTP API
                  │
                  └─ command handlers (deterministic)
                  └─ summarizer (optional LLM)
```

`teloxide` long-polls (or webhooks) for updates; each command handler maps to
one or more daemon IPC calls.

### Commands

| Command | Daemon call | LLM? |
|---------|-------------|------|
| `/status` | `ListSessions` + breaker gauges | no (raw) / optional summary |
| `/sessions` | `ListSessions` | no |
| `/start <path>` | `StartSession` | no |
| `/stop <id>` | `StopSession` | no |
| `/approve <id>` | `ResolveApproval{approved:true}` | no |
| `/deny <id>` | `ResolveApproval{approved:false}` | no |
| `/report` | aggregate state | yes — LLM summary |

### Permission approval flow

1. A session hits a permission gate → daemon sets status
   `AwaitingApproval` and emits an event.
2. The Telegram bot, subscribed to the event stream, pushes a message with
   inline buttons: **Approve** / **Deny**.
3. Operator taps a button → callback → `Request::ResolveApproval`.
4. Daemon unblocks the session; bot edits the message to show the outcome.

This is fully deterministic — no model decides the verdict.

## AI Commander pattern

Management is deterministic; **summarization is optional and LLM-powered**.

- All start/stop/approve/status operations are pure API calls. The system
  works with the LLM disabled.
- `/report` (and an optional TUI "summary" pane) sends the current aggregate
  state to a model and asks for a concise natural-language briefing:
  *"3 sessions active, planner delegation on /proj/foo running 4 min, context
  breaker at 82% — consider compaction soon."*
- The LLM is a **read-only narrator**, never a controller. It cannot start,
  stop, or approve anything. This keeps the control plane safe and testable
  while still giving humans a friendly status surface.
