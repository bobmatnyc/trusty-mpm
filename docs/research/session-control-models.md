# Session Control Models

trusty-mpm must host Claude Code processes so the daemon can observe output,
inject input, and detach/reattach. Three models were evaluated.

## Option A — tmux sessions

Spawn Claude Code inside a named tmux session; drive it with
`tmux send-keys`, `capture-pane`, `attach`, `detach`.

**Pros**
- Free detach/reattach: a human can `tmux attach -t trusty-mpm-<id>` and see
  exactly what the daemon sees.
- Survives daemon restarts — the session keeps running.
- Trivial multi-session: one tmux session per Claude Code session.
- No PTY plumbing in Rust; tmux owns the terminal.
- Scriptable and battle-tested.

**Cons**
- Hard dependency on the `tmux` binary.
- Output capture is poll-based (`capture-pane`) rather than a clean stream.
- Parsing pane text is fragile vs. structured output.

## Option B — daemon-owned PTY

Daemon allocates a PTY (`openpty`) and runs Claude Code as a direct child;
crates such as `portable-pty` provide the primitives.

**Pros**
- Clean byte stream in and out — no screen-scraping.
- No external binary dependency.
- Full control over resize, signals, environment.

**Cons**
- Session dies if the daemon dies (no external supervisor).
- Reattach must be reimplemented (multiplex the PTY to attaching clients).
- More Rust surface area to get right (raw mode, escape handling).

## Option C — Claude Code SDK / headless mode

Run Claude Code non-interactively (`claude -p` / headless / SDK), exchanging
structured JSON instead of terminal I/O.

**Pros**
- Structured I/O — no terminal parsing at all.
- Best fit for deterministic automation and the Telegram bot.
- Cleanest hook/event integration.

**Cons**
- No interactive attach for a human mid-session.
- Feature surface differs from the interactive TUI.
- Long-running interactive flows are awkward.

## Comparison

| Criterion | tmux | PTY | SDK |
|-----------|:----:|:---:|:---:|
| Human attach/detach | ✅ | ⚠️ (build it) | ❌ |
| Survives daemon restart | ✅ | ❌ | ❌ |
| Structured I/O | ❌ | ⚠️ | ✅ |
| External dependency | tmux | none | claude CLI |
| Implementation cost | low | high | medium |
| Multi-session | ✅ | ✅ | ✅ |

## Recommendation

**Primary: tmux.** It delivers detach/reattach and restart-survival for
near-zero implementation cost — the highest-value properties for an operator
tool. The daemon names sessions `trusty-mpm-<session-id>` and uses
`send-keys` / `capture-pane`.

**Fallback: PTY.** When `tmux` is unavailable, the daemon allocates its own
PTY via `portable-pty`. Same `SessionManager` trait, different backend.

**Special-purpose: SDK mode.** Used for headless/automated runs and for
Telegram-driven one-shot tasks where no human attach is needed.

This maps directly to `ControlModel::{Tmux, Pty, Sdk}` in
`trusty-mpm-core::session`. The `SessionManager` trait abstracts the three so
callers are backend-agnostic.

## Implementation notes

- tmux backend: shell out via `tokio::process::Command`; poll `capture-pane -p`
  on an interval and diff for new output.
- PTY backend: `portable-pty` + a reader task feeding a broadcast channel so
  multiple clients (TUI, Telegram) can observe one session.
- SDK backend: `tokio::process::Command` with `--output-format stream-json`,
  parsing line-delimited JSON.
- All three emit the same internal `SessionEvent` stream so the hook
  interceptor and dashboard never need to know the backend.
