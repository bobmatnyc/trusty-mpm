# CLAUDE.md — AI Orientation for trusty-mpm

trusty-mpm is a **Rust reimagining of claude-mpm** (Claude Multi-Agent Project Manager). It replaces claude-mpm's per-invocation Python process model with a **single stable daemon** (`trusty-mpmd`) that owns sessions, hooks, artifact serving, and trusty service clients.

---

## Workspace Crates

| Crate | Binary | Role |
|---|---|---|
| `trusty-mpm-core` | (lib) | Artifact model, session types, IPC protocol, instruction pipeline |
| `trusty-mpm-daemon` | `trusty-mpmd` | Resident daemon: sessions, hooks, artifact store, trusty clients |
| `trusty-mpm-cli` | `tm` / `trusty-mpm` | Thin IPC client |
| `trusty-mpm-tui` | `trusty-mpm-tui` | ratatui dashboard |
| `trusty-mpm-telegram` | `trusty-mpm-telegram` | Telegram remote-management bot |
| `trusty-mpm-mcp` | (lib) | MCP server implementation |
| `trusty-mpm-gui` | — | Tauri GUI (`publish = false`) |

All crates live under `crates/`.

---

## Key Source Files

| File | Purpose |
|---|---|
| `crates/trusty-mpm-core/src/session_launch.rs` | `prepare_session()`: writes hooks, MCP config, output style into project `.claude/` and `.mcp.json` |
| `crates/trusty-mpm-core/src/instruction_pipeline.rs` | `assemble_system_prompt()`: builds PM system prompt from bundled assets |
| `crates/trusty-mpm-core/src/bundle.rs` | `include_str!` constants for all bundled assets |
| `crates/trusty-mpm-core/src/hook.rs` | `HookEvent` enum (32 events) |
| `crates/trusty-mpm-core/assets/instructions/` | Bundled instruction files (see below) |

---

## Instruction Pipeline

When `tm launch` starts a session, the PM system prompt is assembled in this order:

1. `PM_INSTRUCTIONS.md` — orchestration rules, circuit breakers, delegation routing
2. `WORKFLOW.md` — 5-phase workflow
3. `AGENT_DELEGATION.md` — agent routing table
4. `BASE_PM.md` — non-overridable floor (identity, trusty tool priority with qualified MCP names)

All files are bundled at compile time from `crates/trusty-mpm-core/assets/instructions/`. Also present: `INSTRUCTIONS.md`, `CLAUDE.md`.

Project-level overrides: `.trusty-mpm/INSTRUCTIONS.md` (and sibling files in `.trusty-mpm/`).

---

## Trusty Services

| Service | Port | Purpose |
|---|---|---|
| `trusty-memory` | 3038 | Palace-based memory |
| `trusty-search` | 7878 | Hybrid BM25+vector code search |

Both are native Rust HTTP clients inside the daemon — not subprocess MCP servers.

**MCP tool names (qualified):**
- `mcp__trusty-memory__memory_recall`, `mcp__trusty-memory__memory_store`
- `mcp__trusty-search__search_code`, `mcp__trusty-search__search_health`, `mcp__trusty-search__list_indexes`

---

## How to Search This Codebase

**Always use trusty-search before grep.** The index `trusty-mpm` covers all source and docs.

```
mcp__trusty-search__search_code
  index_id: "trusty-mpm"
  query: "your question here"
```

Fall back to `grep -r` only if trusty-search is unavailable.

---

## Development Workflow

```bash
# Quality gate (must pass before PR)
make check          # cargo test --workspace + cargo clippy -- -D warnings + cargo fmt --check

# Version management
make version-patch  # bump patch version
make version-minor  # bump minor version
make version-major  # bump major version

# Release
make release-patch  # publishes to crates.io (trusty-mpm-gui excluded)
```

**Layer priority for new features:** API (core/daemon) -> CLI -> TUI -> GUI

---

## Architecture Docs

| File | Topic |
|---|---|
| `docs/architecture/instruction-pipeline.md` | Instruction assembly pipeline |
| `docs/architecture/agent-inheritance.md` | Agent composition system |
| `docs/architecture/delegation-authority.md` | Dynamic agent routing |
| `docs/architecture/overseer.md` | Daemon overseer design |
| `docs/research/architecture-overview.md` | Component map and lifecycle |
| `docs/research/trusty-integration.md` | trusty-memory/search client details |
| `docs/research/artifact-compatibility.md` | claude-mpm compatibility contract |
