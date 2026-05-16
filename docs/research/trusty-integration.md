# trusty-memory & trusty-search Integration

claude-mpm reaches trusty-memory and trusty-search as MCP servers spawned as
subprocesses. trusty-mpm replaces this with **native Rust clients** living
inside the daemon — no subprocess, no MCP round-trip overhead for the daemon's
own calls.

## Current state (claude-mpm)

- **trusty-memory** — MCP server on port `3038`.
- **trusty-search** — MCP server on port `7878`.

Both are reached via MCP `tools/call` over a subprocess transport. Claude Code
itself still talks to them as MCP servers; that stays. What changes is the
**daemon's own** access path.

## trusty-mpm approach

Both trusty services already expose HTTP APIs (they ship axum servers — see
`trusty-search` Cargo.toml `axum-server` feature, `trusty-memory` axum dep).
The daemon talks to them directly over HTTP with `reqwest`.

```
trusty-mpmd
├── MemoryClient  ──HTTP──▶ trusty-memory  :3038
└── SearchClient  ──HTTP──▶ trusty-search  :7878
```

Two consumers, two paths:

| Consumer | Path | Why |
|----------|------|-----|
| Claude Code session | MCP server (unchanged) | model needs MCP tool surface |
| trusty-mpm daemon | native HTTP client | management/observability, no MCP tax |

## Client design

Both clients live in the daemon crate behind small async traits so they can be
faked in tests.

```rust
#[async_trait]
trait MemoryClient {
    async fn health(&self) -> Result<MemoryHealth>;
    async fn stats(&self) -> Result<MemoryStats>;       // node count, etc.
    async fn recall(&self, query: &str) -> Result<Vec<MemoryHit>>;
    async fn store(&self, entry: MemoryEntry) -> Result<()>;
}

#[async_trait]
trait SearchClient {
    async fn health(&self) -> Result<SearchHealth>;
    async fn indexes(&self) -> Result<Vec<IndexInfo>>;
    async fn search(&self, index: &str, query: &str) -> Result<Vec<SearchHit>>;
}
```

- `HttpMemoryClient` / `HttpSearchClient` — `reqwest` implementations.
- A connection is established at daemon start; health is polled and surfaced
  in the TUI / `/status`.
- Endpoints and ports are configurable (`TRUSTY_MEMORY_ADDR`,
  `TRUSTY_SEARCH_ADDR`) with the `3038` / `7878` defaults.

## Why native clients

- **No subprocess** — the daemon does not spawn an MCP server to query memory.
- **Shared connection** — one keep-alive HTTP client, reused across hooks.
- **Typed surface** — request/response structs instead of stringly-typed MCP
  tool payloads.
- **Observability** — health, latency, and stats feed straight into the
  dashboard.

## Usage inside the daemon

- **Hook enrichment** — on `UserPromptSubmit`, the daemon can `recall()`
  relevant memory and `search()` relevant code, then prepend context before
  forwarding the prompt (an OOB enrichment, complementing skill resolution).
- **Session memory** — on `SessionStart` / `Stop`, persist session outcomes
  via `store()`.
- **Dashboard** — `stats()` / `indexes()` populate the trusty pane.

## Resilience

- Calls are wrapped with a timeout and a circuit breaker; if trusty-memory or
  trusty-search is down, the daemon degrades gracefully (skips enrichment,
  marks the service red in the TUI) rather than failing the session.
- Local-dev override pattern mirrors the sibling crates: when `../trusty-*`
  checkouts exist, point at local builds; otherwise use released services.
