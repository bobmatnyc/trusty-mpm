# Dashboard & Telegram

Two operator interfaces sit on top of the daemon HTTP API: a local ratatui TUI
and a Telegram bot for remote management. Both are thin clients — all state
lives in the daemon.

## Multi-session dashboard (ratatui)

The dashboard is a **superset of the claude-mpm dashboard**: the same metrics,
but across *N* concurrent sessions instead of one.

### Panels

| Panel | Shows |
|-------|-------|
| **Sessions** | Every active session: id, workdir, status, uptime, token usage, current agent, last activity |
| **Agents** | Per-session delegation tree: active delegations, model tier, circuit-breaker state per agent |
| **Files** | Cross-session file monitor — files changed across every watched session workdir |
| **Event feed** | Live stream of hook events per session (all 32 event types) |
| **Memory** | Per-session context-window pressure as a gauge/bar (amber at warn, red at alert) |

### Why a superset

claude-mpm's dashboard watches the single session you launched. trusty-mpm runs
a daemon that may host many sessions at once (a PM session, several worktree
sessions, scripted SDK runs). The dashboard therefore lists *all* of them, each
with its own delegation tree and memory gauge — the operator sees the whole
fleet on one screen.

### Implementation

- **`trusty-mpm-tui::client`** — `DaemonClient` polls the daemon HTTP API
  (`/health`, `/sessions`, later `/events` and `/breakers`).
- **`trusty-mpm-tui::dashboard`** — pure rendering. `DashboardState` holds the
  polled rows; `session_rows()` builds the table rows (unit-tested without a
  terminal); `render()` draws the ratatui frame.
- **`main`** — terminal setup/teardown wraps `run_loop`, which polls on a timer,
  redraws, and quits on `q` / `Esc`.

Keeping HTTP and rendering in separate modules means the dashboard logic is
testable without a daemon or a terminal.

### Agent monitoring

Each session row expands into its delegation tree, reconstructed from the flat
`Delegation` list using each node's `parent` link (`DelegationId`). Every node
shows its `ModelTier` (haiku/sonnet/opus, colour-coded) and the agent's
`CircuitState` (closed/open/half-open) so a tripped breaker is visible at a
glance.

### File monitoring

The daemon's `FileWatcher` registers one watch root per session workdir. A
filesystem change is attributed to the session whose root is the *longest*
matching prefix of the changed path (so nested projects resolve correctly), then
synthesised into a `FileChanged` hook event. The dashboard's Files panel is
therefore just another consumer of the same hook feed.

## Telegram bot

Remote management from a phone: list sessions, check status, approve or deny a
pending permission request, and receive alerts.

### Commands

`trusty-mpm-telegram::commands` parses chat text into a typed `BotCommand`:

| Command | Action |
|---------|--------|
| `/sessions` | List all managed sessions |
| `/status <id>` | Detailed status for one session |
| `/approve <id>` | Approve a session's pending permission request |
| `/deny <id>` | Deny a session's pending permission request |
| `/help` | Command list |

Parsing is pure and exhaustively unit-tested (missing argument, extra argument,
unknown command) so the teloxide dispatch layer stays trivial.

### Permission approval flow

When a session blocks on a permission request its status becomes
`AwaitingApproval`. The bot pushes a notification; the operator replies
`/approve <id>` or `/deny <id>`; the bot forwards the decision to the daemon,
which resolves the request and unblocks the session. No LLM is involved — the
flow is fully deterministic.

### Alerts and event subscription

`trusty-mpm-telegram::alerts` decides *what* to push and *how* to format it:

- **`AlertConfig`** — which `HookCategory` values the operator subscribed to,
  plus a memory-alerts toggle. The `recommended()` default subscribes the
  `Permission` and `Agent` categories and enables memory alerts.
- **`should_alert`** — the subscription filter; checks an event's category
  against the subscribed set. 32 raw events firing per tool call would spam the
  chat, so the operator opts in by category.
- **`should_memory_alert`** — only `Alert` and `Compact` pressure levels are
  worth interrupting the operator; `Warn` stays on the dashboard.
- **`format_memory_alert` / `format_event_alert`** — short, glanceable message
  bodies naming the session and the event/pressure level.

#### Memory protection alerting

When any session crosses the `alert_at` threshold (default 85%) the daemon
classifies it `MemoryPressure::Alert`; the bot pushes a memory alert naming the
session and the percentage. At `compact_at` (90%) the daemon auto-compacts and
the alert reflects the `Compact` level. The TUI shows the same pressure on its
memory gauge — both read the one `pressure()` classification from
`trusty-mpm-core::memory`.

## Deterministic control, optional summarization

Both interfaces follow the "AI Commander" principle: **management is
deterministic** (start/stop/status/approval need no LLM), and an LLM is used
only optionally to turn the raw daemon state into a human-friendly status
summary. The provider-agnostic chat in `trusty-common` supplies that
summarization when enabled.

## Status

Implemented: the multi-session dashboard rendering and HTTP client, the Telegram
command parser, and the alert filter/formatter — all unit-tested.
Pending: the live ratatui agent/file/memory panels wired to `/events` and
`/breakers`, and the teloxide runtime (long-polling dispatch + alert pusher) —
tracked in the GitHub issues.
