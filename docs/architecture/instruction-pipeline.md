# Instruction Build Pipeline

Every time a Claude Code session starts under trusty-mpm management, the
framework constructs the effective system instructions by merging several
sections in a defined order. This build step determines what the new CC instance
knows about itself, its environment, and the agents it can delegate to. The
result is what the instance actually receives — not any individual source file.

## The Pipeline

Session start executes five steps. Steps 1–2 are the agent build/deploy phase
(documented in [Agent Inheritance](./agent-inheritance.md)). Steps 3–5 are the
instruction merge:

```
Step 1: Build agent compositions from source files
Step 2: Deploy composed agents to ~/.claude/agents/ (respecting ownership)
─────────────────────────────────────────────────────
Step 3: INSTRUCTIONS.md       ← framework standing instructions (always present)
Step 4: delegation authority  ← dynamically built from deployed agents
Step 5: CLAUDE.md             ← project-specific stub (user-owned)
─────────────────────────────────────────────────────
→ Merged instruction set passed to the new Claude Code instance
```

Each step's output is appended to the previous. The resulting document is
delivered to the CC instance as its initial context. The instance never sees
the individual source sections — it sees the merged whole.

## Section 3: INSTRUCTIONS.md

**Source**: `~/.trusty-mpm/framework/instructions/INSTRUCTIONS.md`
**Install policy**: `Overwrite` — replaced on every `trusty-mpm install` run
**Runtime modifications**: none — read-only at session time

This is the framework's standing instruction set for all managed instances.
It covers:

- How to communicate with the daemon (the HTTP endpoint, MCP tools available)
- Session lifecycle behavior (how to signal completion, how to handle handoffs)
- Where framework configuration lives and how to modify it
- What the reversibility guarantees are for configuration changes

Because `INSTRUCTIONS.md` is compiled into the trusty-mpm binary as a bundled
asset (via `include_str!` in `trusty-mpm-core::bundle`), every install deploys
the version that matches the installed binary. Framework upgrades automatically
refresh this file. Users should not edit it — changes would be overwritten on
the next `trusty-mpm install`.

The file is embedded as a compile-time constant:

```rust
// trusty-mpm-core/src/bundle.rs
pub const FRAMEWORK_INSTRUCTIONS: &str =
    include_str!("../assets/instructions/INSTRUCTIONS.md");
```

Its install artifact entry uses `InstallPolicy::Overwrite`:

```rust
BundledArtifact {
    rel_path: "instructions/INSTRUCTIONS.md",
    contents: FRAMEWORK_INSTRUCTIONS,
    install: InstallPolicy::Overwrite,
}
```

## Section 4: Delegation Authority

**Source**: generated fresh at each session start by scanning `~/.claude/agents/`
**Install policy**: not a file — built in memory and injected at merge time
**Runtime modifications**: none once generated for a session

The delegation authority tells the orchestrating CC instance which agents are
available and how to route work to them. Because it is generated from the
deployed agent directory rather than a static file, it reflects the actual
available agents at session time. See [Delegation Authority](./delegation-authority.md)
for the full specification of what gets scanned and what the output contains.

## Section 5: CLAUDE.md

**Source**: `~/.trusty-mpm/framework/instructions/CLAUDE.md`
**Install policy**: `SeedOnce` — written only if the file does not already exist
**Runtime modifications**: allowed and encouraged; this is the user's file

This is the user's section. It is seeded once — by `trusty-mpm install` (on
first install) or by `trusty-mpm project init` (when registering a project) —
and never touched again by the framework. Everything in it is under the user's
control.

The install artifact entry uses `InstallPolicy::SeedOnce`:

```rust
BundledArtifact {
    rel_path: "instructions/CLAUDE.md",
    contents: CLAUDE_STUB,
    install: InstallPolicy::SeedOnce,
}
```

The shipped stub is intentionally minimal:

```markdown
# Project-specific instructions — customize for your project

## About this project

## Conventions
```

Users fill this in with project conventions, codebase notes, technology
choices, or any standing instructions that should apply to every CC instance
launched for this project. This is the right place for things like:

- "This project uses Rust edition 2024; clippy lints are enforced in CI"
- "All database migrations live in `db/migrations/` and must be reversible"
- "Never commit directly to `main`; open a PR and request review"

## Behavioral Policies and File-Backed Changes

`INSTRUCTIONS.md` explicitly tells instances where framework configuration
lives. This enables an important pattern: instances can modify their own
behavioral policies by editing config files, and those changes take effect
without a session restart.

The primary example is the token optimizer policy:

```
~/.trusty-mpm/framework/hooks/optimizer.toml
```

This TOML file controls how aggressively the daemon compresses tool outputs
before they reach the CC instance. An instance that observes it is consuming
context budget too quickly can lower the compression threshold. The daemon's
file watcher picks up the change and applies the new policy immediately.

Reversibility is straightforward:

- **Edit the config file** → the file watcher detects the change → behavior
  updates within one watcher cycle (typically under a second)
- **Run `trusty-mpm install --force`** → all framework files with
  `InstallPolicy::Overwrite` (including `INSTRUCTIONS.md` and `optimizer.toml`)
  are reset to bundled defaults; `CLAUDE.md` is never reset

This two-tier model — framework files that reset on install, user files that
never reset — means users can always recover to a known-good baseline without
losing project-specific customizations.

## File Paths Quick Reference

```
~/.trusty-mpm/
├── framework/
│   ├── agents/          ← agent source files (extended by inheritance)
│   ├── skills/          ← skill source files
│   ├── hooks/
│   │   └── optimizer.toml   ← behavioral policy (overwritten on install)
│   └── instructions/
│       ├── INSTRUCTIONS.md  ← framework standing instructions (overwritten on install)
│       └── CLAUDE.md        ← project stub (seeded once, user-owned)
└── registry/            ← installed packages from public registries

~/.claude/
└── agents/
    ├── .trusty-mpm-manifest.json  ← ownership manifest
    ├── engineer.md      ← composed agent (built from inheritance chain)
    ├── research.md      ← composed agent
    └── ...
```

## Future Optimization Opportunities

The current pipeline rebuilds all sections unconditionally at every session
start. Potential optimizations that have been identified but not yet
implemented:

- **Section hashing**: hash each input section and skip the merge if all input
  hashes match the previous run's recorded hashes. Useful when session starts
  are frequent and the framework files change rarely.
- **Lazy authority generation**: if the active workflow does not use delegation
  (e.g., a single-agent session), skip scanning `~/.claude/agents/` and omit
  the delegation authority section.
- **Named section registry**: maintain a registry of named sections so that
  third-party plugins can inject content at a specific position in the merge
  order rather than always appending.

These optimizations are straightforward to add without changing the external
interface. The session start pipeline's numbered step structure is intentional:
it provides clear insertion points for new sections and clear hooks for caching
logic.

## See Also

- [Agent Inheritance System](./agent-inheritance.md) — steps 1–2, the agent
  build/deploy phase that must complete before this pipeline runs
- [Delegation Authority](./delegation-authority.md) — the detailed specification
  for section 4, the dynamically generated routing document
