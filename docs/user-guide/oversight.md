# Session Oversight

When you run multiple Claude Code sessions at once, you cannot watch all of them.
Sessions ask questions, hit dead ends, encounter auth prompts, and sometimes start
doing things they shouldn't. The overseer is the trusty-mpm component that watches
your sessions and handles routine decisions on your behalf — so you can start work
and come back to results rather than baby-sitting a terminal.

## What the Overseer Does

The overseer sits between your sessions and the outside world. It sees every tool
call a session makes before the call executes, and every response that comes back.
For each event it makes a decision: approve, block, respond, or flag for your
review.

Most events are handled without involving you at all. A session asks "shall I
proceed?" — the overseer answers yes. A session tries to run `rm -rf /` — the
overseer blocks it before it runs. A session burns through its token budget — the
overseer warns it or stops it. You review the log later and see exactly what
happened.

Items that genuinely need a human decision surface in the TUI oversight queue,
where you approve or reject with a single keypress.

## Two Modes

### Deterministic Mode (Always Available)

Deterministic mode uses rules you configure — blocklists, allowlists, auto-response
patterns, rate limits, budget caps. It makes decisions instantly, requires no API
keys, and adds no perceptible latency to your sessions.

This mode is the safety floor. It runs regardless of whether you have an LLM
configured, and its rules always take effect first.

### LLM-Assisted Mode (Optional)

When you configure an LLM provider (any provider that offers an API key), the
overseer gains a judgment layer for situations where rules alone aren't enough.
The LLM only sees events that the deterministic rules couldn't resolve clearly —
an unusual command that isn't on either list, an ambiguous question that doesn't
match any auto-response pattern.

The LLM is never consulted for events deterministic rules can handle. If an event
is clearly blocked by the blocklist, it's blocked immediately. If it's clearly
approved by the allowlist, it's approved immediately. The LLM only comes in for
genuinely ambiguous cases.

This keeps cost predictable and latency low even when LLM mode is active.

## What the Overseer Protects Against

**Dangerous commands.** The blocklist stops matching tool calls before they
execute. Common defaults include `rm -rf`, `git push --force main`, and destructive
SQL statements. You add your own patterns in `overseer.toml`.

**Runaway sessions.** The rate limiter caps tool calls per minute. The circuit
breaker halts a session that is spinning on repeated errors. Both prevent a
session from doing significant damage while you're not watching.

**Budget overruns.** The budget enforcer tracks token consumption and warns or
stops sessions that exceed configured limits.

**Stuck auth prompts.** When a session encounters an authentication challenge for
a known credential, the overseer injects it automatically. The session doesn't
stall waiting for you.

## Auto-Responses

Sessions frequently ask questions that have obvious answers: "shall I proceed?",
"should I commit?", "want me to continue with the next file?". You can configure
the overseer to answer these automatically.

In `overseer.toml`:

```toml
[auto_responses]
"shall I proceed" = "yes, proceed"
"should I commit" = "yes, commit with an appropriate message"
"want me to continue" = "yes, continue"
```

Pattern matching is substring-based and case-insensitive. The overseer writes the
configured response directly into the tmux session — the session reads it as if
you had typed it. No special setup is needed in the session; the communication
channel is the terminal itself.

## LLM Mode Setup

Add an `[llm]` section to your `overseer.toml`:

```toml
[llm]
provider = "anthropic"
model = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"  # pragma: allowlist secret
```

The `api_key_env` field names an environment variable that holds your API key —
the key itself is never written to the config file. Any provider with a compatible
API works; Anthropic is shown as an example.

With this in place, ambiguous events are escalated to the LLM. The LLM receives
the event details and recent session context, then returns an approve, reject,
respond, or flag-for-human decision. That decision is logged alongside
deterministic decisions in the same audit log.

To return to deterministic-only mode, remove or comment out the `[llm]` section.

## Audit Log

Everything the overseer decides is written to
`~/.trusty-mpm/logs/overseer/YYYY-MM-DD.jsonl`. One line per decision, one file
per day. The log is append-only — nothing is ever modified or deleted.

Each record shows the session name, the event type, what the overseer decided,
why, and which component made the call (deterministic rule, auto-responder, or
LLM). If something unexpected happened in a session, this is where you look first.

## TUI Oversight Queue

The TUI shows an oversight queue alongside the session list. Three types of items
appear there:

**Flagged** — events the overseer could not resolve automatically. These require
your input. The session waits until you approve or reject. Use the TUI to act
without switching to the session's terminal.

**Auto-handled** — events the overseer resolved without your involvement. These
are shown briefly for awareness and then dismissed. They're always in the audit
log if you want to review them later.

**Session health** — rate limit proximity, budget consumption, circuit breaker
state. Lets you see at a glance which sessions are approaching their limits.

## Reversibility

All oversight policy is in `~/.trusty-mpm/framework/hooks/overseer.toml`. Editing
the file takes effect immediately — the daemon watches for changes. If you want to
reset to the defaults that shipped with trusty-mpm, run:

```
trusty-mpm install --force
```

This reinstalls the default config. Your session history and audit logs are
unaffected.

## Quick Start

### Enable the Overseer

In `~/.trusty-mpm/framework/hooks/overseer.toml`, set:

```toml
[overseer]
enabled = true
```

That's it. Deterministic oversight is active for all new and running sessions.

### Add a Blocklist Entry

```toml
[deterministic]
blocklist = ["rm -rf", "git push --force main", "DROP TABLE", "kubectl delete namespace"]
```

Add any substring you want to block. The match is applied to the full tool input
before execution.

### Configure LLM Mode

```toml
[llm]
provider = "anthropic"
model = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"  # pragma: allowlist secret
```

Export `ANTHROPIC_API_KEY` in your shell environment (or add it to the trusty-mpm
env config), then restart the daemon. Ambiguous events will now escalate to the
LLM rather than landing in the TUI queue.
