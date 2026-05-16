# trusty-mpm

A Rust reimagining of [`claude-mpm`](https://github.com/bobmatnyc/claude-mpm)
as a **stable, long-running daemon** with a richer control model.

## What it is

`trusty-mpm` orchestrates Claude Code subagents — agent delegation, skills,
hooks, and MCP integration — but replaces claude-mpm's per-invocation Python
process model with a single resident daemon (`trusty-mpmd`).

## Why

claude-mpm spawns a fresh Python process for every Claude Code hook event.
A resident daemon eliminates the startup tax, keeps shared state in memory,
and holds persistent connections to the trusty services.

| | claude-mpm | trusty-mpm |
|--|------------|------------|
| Model | Python CLI, process-per-hook | Rust daemon, in-process hooks |
| Hooks | wired via `settings.json` | intercepted out-of-band by the daemon |
| Agents/Skills | scattered `.md` files | served from a daemon artifact store |
| trusty-memory/search | MCP subprocess | native Rust HTTP clients |
| Interfaces | FastAPI service | axum API + ratatui TUI + Telegram bot |

## Goals

- **Stable daemon** — one process, shared state, fast hook response.
- **Flexible session control** — tmux (default), PTY fallback, or SDK/headless.
- **Out-of-band artifacts** — agents, skills, and hooks managed by the daemon,
  **100% compatible** with claude-mpm's `.md` frontmatter and hook event names.
- **Native trusty integration** — direct clients to trusty-memory (`:3038`)
  and trusty-search (`:7878`).
- **Operator interfaces** — a ratatui dashboard and a Telegram bot for remote
  management, with deterministic control and optional LLM status summaries.

## Workspace layout

```
crates/
├── trusty-mpm-core      # shared types: artifacts, sessions, IPC protocol
├── trusty-mpm-daemon    # trusty-mpmd: the resident daemon
├── trusty-mpm-cli       # trusty-mpm: thin CLI client
├── trusty-mpm-tui       # trusty-mpm-tui: ratatui dashboard
└── trusty-mpm-telegram  # trusty-mpm-telegram: Telegram remote bot
docs/research/           # architecture & design research
```

## Build

```sh
cargo build --workspace
cargo test  --workspace
cargo run -p trusty-mpm-daemon      # starts the daemon on 127.0.0.1:7880
```

Toolchain: stable Rust, edition 2021 (pinned via `rust-toolchain.toml`).

## Status

Early scaffold (`v0.1.0` — "Foundation" milestone). See `docs/research/` for
the design foundation and the GitHub issue tracker for the implementation plan.

## Relationship to claude-mpm

trusty-mpm is a clean-room Rust successor. It is artifact-compatible with
claude-mpm — existing agents, skills, and hook configurations work unchanged —
but is operationally independent. See
[`docs/research/architecture-overview.md`](docs/research/architecture-overview.md).

## Research docs

- [`architecture-overview.md`](docs/research/architecture-overview.md) — daemon architecture & component map
- [`session-control-models.md`](docs/research/session-control-models.md) — tmux vs PTY vs SDK
- [`artifact-compatibility.md`](docs/research/artifact-compatibility.md) — OOB artifact handling
- [`dashboard-telegram.md`](docs/research/dashboard-telegram.md) — TUI & Telegram design
- [`trusty-integration.md`](docs/research/trusty-integration.md) — trusty-memory/search clients

## License

MIT
