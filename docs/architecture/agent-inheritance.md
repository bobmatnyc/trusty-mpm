# Agent Inheritance System

trusty-mpm implements a three-level agent inheritance chain that Claude Code
does not natively support. Claude Code reads only flat Markdown files from
`~/.claude/agents/`; it has no awareness of parent types, `extends` fields, or
composition. Resolving the chain and producing fully self-contained agent files
is entirely trusty-mpm's responsibility, performed at session-start time.

## Overview

Agent definitions are organized as a class hierarchy. A leaf agent file
describes only the things that are specific to that role. Every behavior it
shares with other agents — memory routing, git workflow conventions, output
format standards, handoff protocol — lives in a parent type and is inherited at
build time rather than duplicated across files.

The canonical three-level chain for the engineer role:

```
BASE-AGENT.md          ← foundation layer (memory, git, output, handoff)
    └── BASE-ENGINEER.md   ← engineer foundation (code quality, testing, linting)
            └── engineer.md    ← role definition (scope, tools, constraints)
```

Other built-in roles follow the same pattern:

```
BASE-AGENT ← BASE-RESEARCH ← research.md
BASE-AGENT ← BASE-QA      ← qa.md
BASE-AGENT ← BASE-OPS     ← ops.md
```

Each level adds specificity. Nothing in a parent file needs to be repeated in a
child file.

## Frontmatter Convention

Every agent source file carries a YAML frontmatter block that declares its
position in the hierarchy. The `extends` field names the immediate parent type.
The `role` field becomes the deployed filename.

```yaml
---
extends: base-engineer
role: engineer
---

# Engineer

This agent implements features, fixes bugs, and performs refactoring...
```

A base type that itself has a parent:

```yaml
---
extends: base-agent
role: base-engineer
---

# Base Engineer

All engineer roles inherit these code-quality and testing standards...
```

The root of the hierarchy (`BASE-AGENT`) has no `extends` field.

## Build Resolution

At session start, trusty-mpm walks the `extends` chain for every agent source
file and concatenates the results into a single Markdown document. The
resolution algorithm:

```
1. Read the leaf agent file from ~/.trusty-mpm/framework/agents/
2. Parse the `extends:` field from its frontmatter
3. Recursively resolve the parent chain until a file with no `extends` is reached
4. Concatenate in order: root first, leaf last
   BASE-AGENT.md content
   + BASE-ENGINEER.md content
   + engineer.md content
5. Strip all frontmatter blocks from the composed output
6. Write the composed, self-contained file to ~/.claude/agents/engineer.md
```

Base content appears before specific content. Because Claude Code reads the file
top to bottom, specifics at the end can override or refine anything in the
foundation layers. Claude Code never sees the source files, the frontmatter, or
the inheritance chain — it sees only the final composed document.

## Source vs. Deployed Locations

```
~/.trusty-mpm/framework/agents/   ← source files (managed by trusty-mpm install)
~/.claude/agents/                  ← deployed files (built by trusty-mpm session start)
```

Source files are the source of truth. The deployed directory contains only build
artifacts. Editing a deployed file directly is possible but the change will be
overwritten the next time `trusty-mpm session start` runs. To make a permanent
change, edit the source file.

## Ownership Manifest

trusty-mpm must never overwrite an agent file that the user has modified
outside of the framework. To track which deployed files it owns, it maintains a
manifest at:

```
~/.claude/agents/.trusty-mpm-manifest.json
```

### Manifest Format

```json
{
  "version": 1,
  "managed": {
    "engineer.md": {
      "source": "base-agent+base-engineer+engineer",
      "checksum": "sha256:abc123...",
      "deployed_at": "2026-05-16T18:30:00Z",
      "origin": "bundled"
    },
    "research.md": {
      "source": "base-agent+base-research+research",
      "checksum": "sha256:def456...",
      "deployed_at": "2026-05-16T18:30:00Z",
      "origin": "bundled"
    }
  }
}
```

Fields:

| Field | Description |
|-------|-------------|
| `source` | The chain of source files that produced this artifact, joined by `+` |
| `checksum` | SHA-256 of the deployed file contents at the time of deployment |
| `deployed_at` | ISO 8601 UTC timestamp of the last successful deploy |
| `origin` | `"bundled"` for framework-shipped agents, `"registry"` for installed ones |

### Deploy Rules

The checksum comparison is the mechanism that prevents clobbering user edits:

| File state in `~/.claude/agents/` | Action |
|------------------------------------|--------|
| Filename is in the manifest and the current file checksum matches the recorded checksum | Safe to overwrite — no user modifications since last deploy |
| Filename is in the manifest but the current file checksum differs | User has modified the file — warn and skip, never overwrite |
| Filename is not in the manifest | User-owned file — never touch under any circumstances |

This means a user can safely edit any agent file that trusty-mpm deployed. On
the next session start, trusty-mpm detects the modification, logs a warning
identifying which file was skipped, and leaves the user's version intact. To
accept framework updates for a modified file, the user deletes their version and
lets trusty-mpm redeploy the composed default.

## User-Defined Agents

Users can write their own agents that extend any trusty-mpm base type. The
frontmatter convention is identical:

```yaml
---
extends: base-engineer
role: my-custom-engineer
---

# My Custom Engineer

Handles only the payments module. Additional constraints:

- Never modify files outside `src/payments/`
- Always add a `#[cfg(test)]` block covering the happy path
```

When `trusty-mpm session start` encounters this file, it resolves the
`extends: base-engineer` chain exactly as it would for the built-in engineer
role. The resulting deployed file at `~/.claude/agents/my-custom-engineer.md`
contains the full BASE-AGENT + BASE-ENGINEER foundation followed by the user's
additional content.

User-defined agent source files can live anywhere the user chooses. A common
pattern is to keep them in the project repository under `.trusty-mpm/agents/`
and point the installer at that directory, so the project's agents are version-
controlled alongside the code.

## Build Consistency Guarantee

Every `trusty-mpm session start` rebuilds and redeploys all trusty-mpm-owned
agents from their source files. There are no incremental or cached builds.
This guarantee means:

- Installing a framework update and starting a new session is sufficient to
  pick up changes to any parent type across all agents that extend it.
- A corrupt or manually edited deployed file is never silently propagated. The
  checksum check either confirms the file is unmodified (safe to replace) or
  detects a user edit (skip and warn).

## Relation to Other Architecture Components

The agent build phase (steps 1–2 of the session-start pipeline) runs before the
instruction pipeline (steps 3–5). The delegation authority builder then reads
the deployed `~/.claude/agents/` directory to learn what agents are available
and what capabilities they advertise. See:

- [Delegation Authority](./delegation-authority.md) — how deployed agents are
  scanned to build the routing table
- [Instruction Build Pipeline](./instruction-pipeline.md) — how the agent build
  phase fits into the overall session-start sequence
