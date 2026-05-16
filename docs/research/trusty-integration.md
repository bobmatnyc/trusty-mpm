# trusty Integration

trusty-mpm integrates with the trusty ecosystem in two ways: it **builds on the
shared `trusty-common` workspace crates**, and it **talks to the trusty-memory
and trusty-search sidecar services** as native Rust HTTP clients.

claude-mpm reaches trusty-memory and trusty-search as MCP servers spawned as
subprocesses. trusty-mpm replaces this with **native Rust clients** living
inside the daemon — no subprocess, no MCP round-trip overhead for the daemon's
own calls.

## trusty-common shared crates

The `trusty-common` workspace (sibling checkout, referenced by path) provides
crates trusty-mpm reuses instead of reinventing:

| trusty-common crate | What trusty-mpm uses it for |
|---------------------|------------------------------|
| `trusty-mcp-core` | JSON-RPC 2.0 / MCP request/response envelopes, standard error codes, the `initialize` payload builder, and `run_stdio_loop` — the foundation of the `trusty-mpm-mcp` server and the `trusty-mpmd mcp` run mode |
| `trusty-common` | Shared utilities and the provider-agnostic streaming chat (`ChatProvider`) used for the optional LLM status summaries in the dashboard / Telegram bot |

These are added to the workspace `Cargo.toml` as path dependencies:

```toml
trusty-common   = { path = "../trusty-common/crates/trusty-common" }
trusty-mcp-core = { path = "../trusty-common/crates/trusty-mcp-core" }
```

Path deps keep trusty-mpm building against local `trusty-common` changes
without a publish cycle. Reusing `trusty-mcp-core` in particular means the MCP
wire format, error codes, and stdio loop are identical to trusty-memory and
trusty-search — there is exactly one implementation of the JSON-RPC envelope in
the ecosystem, and a fix in one place fixes every server.

### Why not vendor the MCP types

`trusty-mpm-mcp` could have defined its own `Request`/`Response`. It does not —
that is precisely the drift-prone pattern `trusty-mcp-core` was created to kill.
trusty-mpm's MCP crate adds only its *tool catalog* and `dispatch` routing on
top of the shared envelopes.

## Current state (claude-mpm)

- **trusty-memory** — MCP server; address read at runtime from
  `~/.trusty-memory/http_addr` (fallback default `127.0.0.1:3038` only when
  the file is absent — never hardcode this port in call sites).
- **trusty-search** — MCP server; address read at runtime from
  `~/.trusty-search/http_addr` (fallback default `127.0.0.1:7878` only when
  the file is absent — never hardcode this port in call sites).

Both are reached via MCP `tools/call` over a subprocess transport. Claude Code
itself still talks to them as MCP servers; that stays. What changes is the
**daemon's own** access path.

## Port discovery

Each trusty service writes its bound address to a file under its data directory
at startup:

| Service | Port file | Example content | Fallback default |
|---------|-----------|-----------------|------------------|
| trusty-memory | `~/.trusty-memory/http_addr` | `127.0.0.1:3038` | `127.0.0.1:3038` (file absent) |
| trusty-search | `~/.trusty-search/http_addr` | `127.0.0.1:7878` | `127.0.0.1:7878` (file absent) |
| trusty-search (MCP) | `~/.trusty-search/mcp_http_addr` | `127.0.0.1:57217` | none — MCP addr is always dynamic |

> **Important:** the `http_addr` file is the source of truth. The fallback
> defaults (3038 / 7878) exist only so new installs work before the service
> has started for the first time. Never embed these numbers directly at call
> sites — always use `discover_addr()` (see `ServiceDiscovery` section below).

The port is **not** configured in the launchd plist — the plist only names the
binary and flags (`start --foreground`). The service picks its port from its
own `config.toml` (or a default) and writes the resolved address to the
`http_addr` file once the listener is bound. Clients must read this file at
runtime rather than using a hardcoded port constant.

Discovery algorithm (used by both daemon clients and claude-mpm's
`migrate_trusty_autodetect`):

1. Read `~/.trusty-{service}/http_addr`.  Parse the `host:port` string.
2. If the file is absent or empty, fall back to the well-known default
   (`127.0.0.1:3038` for memory, `127.0.0.1:7878` for search).
3. Issue an HTTP `GET /health` to verify the service is reachable before
   advertising it as available.

## trusty-mpm approach

Both trusty services already expose HTTP APIs (they ship axum servers — see
`trusty-search` Cargo.toml `axum-server` feature, `trusty-memory` axum dep).
The daemon talks to them directly over HTTP with `reqwest`.

```
trusty-mpmd
├── MemoryClient  ──HTTP──▶ trusty-memory  (addr from ~/.trusty-memory/http_addr)
└── SearchClient  ──HTTP──▶ trusty-search  (addr from ~/.trusty-search/http_addr)
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
- Addresses are resolved at daemon startup via the port-discovery algorithm
  above; environment variables `TRUSTY_MEMORY_ADDR` and `TRUSTY_SEARCH_ADDR`
  override discovery when set, and the well-known defaults are used only as a
  last resort (see `TRUSTY_MEMORY_DEFAULT_ADDR` / `TRUSTY_SEARCH_DEFAULT_ADDR`
  constants — never repeat the literal port numbers at call sites).

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

## ServiceDiscovery

The `ServiceDiscovery` abstraction encapsulates the port-discovery algorithm so
it can be reused across the daemon, CLI, and TUI without duplicating file-read
logic.

### Rust sketch

```rust
/// Resolves the HTTP address for a trusty sidecar service.
///
/// Why: trusty services write their bound address to a well-known file rather
/// than exposing a fixed port, so callers must discover the address at runtime.
/// What: reads `~/{data_dir}/http_addr`, falls back to `default_addr`, then
///       optionally verifies liveness with a GET /health probe.
/// Test: supply a temp dir with a known http_addr file; assert the returned
///       SocketAddr matches its contents.  Supply an absent file; assert the
///       default is returned.
pub async fn discover_addr(
    data_dir: &Path,          // e.g. ~/.trusty-memory
    default_addr: SocketAddr, // e.g. 127.0.0.1:3038
    env_override: Option<&str>,
) -> SocketAddr {
    // 1. Environment variable wins.
    if let Some(raw) = env_override {
        if let Ok(addr) = raw.parse() {
            return addr;
        }
    }

    // 2. Read the service-written port file.
    let port_file = data_dir.join("http_addr");
    if let Ok(contents) = tokio::fs::read_to_string(&port_file).await {
        if let Ok(addr) = contents.trim().parse() {
            return addr;
        }
    }

    // 3. Fall back to the well-known default.
    default_addr
}
```

### Constants (defaults only, never hardcoded at call sites)

```rust
// In trusty-mpm-core or trusty-mpm-daemon config module
const TRUSTY_MEMORY_DEFAULT_ADDR: &str = "127.0.0.1:3038";
const TRUSTY_SEARCH_DEFAULT_ADDR:  &str = "127.0.0.1:7878";
const TRUSTY_MEMORY_DATA_DIR:      &str = ".trusty-memory";
const TRUSTY_SEARCH_DATA_DIR:      &str = ".trusty-search";
```

Call sites resolve addresses once at daemon startup:

```rust
let memory_addr = discover_addr(
    &home.join(TRUSTY_MEMORY_DATA_DIR),
    TRUSTY_MEMORY_DEFAULT_ADDR.parse().unwrap(),
    std::env::var("TRUSTY_MEMORY_ADDR").ok().as_deref(),
).await;

let search_addr = discover_addr(
    &home.join(TRUSTY_SEARCH_DATA_DIR),
    TRUSTY_SEARCH_DEFAULT_ADDR.parse().unwrap(),
    std::env::var("TRUSTY_SEARCH_ADDR").ok().as_deref(),
).await;
```

The resolved addresses are stored in the daemon's `Config` struct and passed
into `HttpMemoryClient` / `HttpSearchClient` at construction — no call site
ever embeds a literal port number.

## claude-mpm Reference

| trusty-mpm Feature | claude-mpm Source | Notes |
|---|---|---|
| trusty-memory MCP integration | `src/claude_mpm/cli/commands/setup/handlers/trusty.py` → `_setup_trusty_memory()` | Installs trusty-memory binary, writes MCP server entry into `.mcp.json`; address discovered at runtime from `~/.trusty-memory/http_addr` (fallback default only if file absent) |
| trusty-search MCP integration | `src/claude_mpm/cli/commands/setup/handlers/trusty.py` → `_setup_trusty_search()` | Installs trusty-search binary, writes MCP server entry; address from `~/.trusty-search/http_addr` (fallback default only if file absent) |
| trusty-analyze MCP integration | `src/claude_mpm/cli/commands/setup/handlers/trusty.py` → `_setup_trusty_analyze()` | Installs trusty-analyze binary, inlined directly into `code-analyzer.md` and `Research` agent frontmatter |
| Auto-detection migration | `src/claude_mpm/migrations/migrate_trusty_autodetect.py` | `run_always` migration: probes binaries + HTTP health on startup, injects MCP entries automatically without manual `setup` |
| MCP service config builder | `src/claude_mpm/services/mcp/config_builder.py` | Builds MCP server JSON entries; service installer in `src/claude_mpm/services/mcp/service_installer.py` |
| MCP service registry | `src/claude_mpm/services/mcp_service_registry.py`, `src/claude_mpm/services/mcp_service_verifier.py` | Tracks which MCP servers are configured; verifies connectivity |
| Memory enrichment in hooks | `src/claude_mpm/hooks/memory_integration_hook.py`, `src/claude_mpm/hooks/kuzu_enrichment_hook.py` | On `UserPromptSubmit`, calls memory service to prepend relevant context; trusty-mpm re-implements via native `MemoryClient::recall()` |
| Memory session persistence | `src/claude_mpm/hooks/kuzu_response_hook.py`, `src/claude_mpm/hooks/kuzu_memory_hook.py` | Stores session outcomes and tool results to Kuzu graph; trusty-mpm maps to `MemoryClient::store()` on `Stop` |
| MCP subprocess transport | `src/claude_mpm/mcp/launcher.py`, `src/claude_mpm/mcp/process_manager.py` | claude-mpm spawns MCP servers as subprocesses; trusty-mpm replaces daemon's own access with direct HTTP via `reqwest` |

## Memory protection ↔ trusty-memory

The memory-protection model (`trusty-mpm-core::memory` — see
`session-control-models.md`) integrates with trusty-memory at the
**auto-compaction boundary**:

1. The daemon tracks per-session token usage in `DaemonState` (fed by
   `TokenUsageUpdate` hook events and the MCP `memory_protect` tool).
2. `MemoryUsage::pressure()` classifies usage against the configurable
   thresholds (warn 70% / alert 85% / compact 90%).
3. At `MemoryPressure::Compact`, **before** triggering Claude Code's
   compaction, the daemon calls `MemoryClient::store()` to snapshot the
   important context into trusty-memory's palace.
4. After compaction the session can `MemoryClient::recall()` that snapshot if
   it needs the dropped context again.

This makes compaction non-lossy: trusty-memory becomes the durable backing
store for context that the model's window can no longer hold. The dashboard
memory gauge and the Telegram memory alert both read the same `pressure()`
classification, so "the daemon is about to compact session X" is visible
everywhere at once.
