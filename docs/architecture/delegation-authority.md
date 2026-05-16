# Delegation Authority

The delegation authority is a structured document that trusty-mpm generates
fresh at the start of every session. It tells the orchestrating Claude Code
instance exactly which agents are available, what each one handles, and how to
route tasks to them. Without it, an orchestrating instance would have no way to
know which specialist agents exist or what they are capable of.

## Why Dynamic Generation

A hardcoded routing table would become stale. Users install new agents from
public registries. Different projects configure different agent sets. A team
member might add a domain-specific agent that the framework has no knowledge of.

By scanning the actual deployed state of `~/.claude/agents/` at session start,
trusty-mpm produces an authority document that is always accurate for the
environment it runs in. There is no synchronization problem between a routing
table and the real agent inventory because the authority is derived from the
inventory at generation time.

## Build Process

The authority builder runs as part of the session-start pipeline, after the
agent build/deploy phase has completed. By that point, `~/.claude/agents/`
reflects the current deployed state: all framework agents are freshly composed
and user-modified agents have been skipped with a warning.

```
Step 1: Agent build/deploy completes (see agent-inheritance.md)
Step 2: Authority builder scans ~/.claude/agents/
  2a. List all .md files (excluding base types and the manifest)
  2b. Parse each file's frontmatter: name, description, model, extends chain
  2c. Resolve capability inheritance from the extends chain
  2d. Generate the authority document
Step 3: Authority document is injected as section 4 of the session instructions
```

Files excluded from the scan:

- `BASE-AGENT.md`, `BASE-ENGINEER.md`, and all other `BASE-*.md` files — these
  are foundation types, not routable agents
- `.trusty-mpm-manifest.json` — the manifest file, not an agent definition

## Frontmatter Fields Used

The authority builder reads these frontmatter fields from each agent file:

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | The agent's identity in delegation calls |
| `description` | yes | One-line summary of the agent's purpose; appears verbatim in the authority |
| `model` | no | Preferred Claude model tier (`haiku`, `sonnet`, `opus`) |
| `extends` | no | Parent type; used to surface capability inheritance |

An agent with a missing or malformed frontmatter block is logged as a warning
and excluded from the authority. This prevents a corrupted agent file from
silently removing routing capability for that agent.

## Output Format

The generated authority section is structured Markdown injected directly into
the session instructions. A representative excerpt:

```markdown
## Available Agents

### engineer
**Model tier**: sonnet
**Inherits from**: base-agent, base-engineer
**Description**: Implements features, fixes bugs, and performs refactoring
within the defined scope of a single work ticket.

**Route to this agent when**:
- A ticket requires code changes in an existing codebase
- Test coverage needs to be added or updated
- A bug has been isolated and needs a fix

**Authority boundary**: Does not make architectural decisions or modify
infrastructure. Escalates to the pm agent for scope changes.

---

### research
**Model tier**: sonnet
**Inherits from**: base-agent, base-research
**Description**: Investigates technical questions, evaluates libraries, and
synthesizes findings into structured reports.

**Route to this agent when**:
- A technical approach needs evaluation before implementation
- A library or API needs investigation
- Root-cause analysis of a non-obvious bug is required

---

### qa
**Model tier**: haiku
**Inherits from**: base-agent, base-qa
**Description**: Writes and runs tests, validates acceptance criteria, and
reports defects with reproduction steps.
```

The orchestrating instance uses this section to make routing decisions. It does
not guess which agents exist or what they do — the authority document is
authoritative.

## Inheritance Awareness

Because trusty-mpm built the agents and knows the `extends` chain for each one,
the authority builder can surface capability inheritance explicitly. An engineer
agent that extends `BASE-ENGINEER` is understood to have all base engineer
capabilities (code quality standards, testing discipline, language patterns)
plus any additions in the leaf file. This information appears in the authority
as the `Inherits from` line and informs the description of routing criteria.

This is more than cosmetic. An orchestrating instance that knows an agent
inherits from `BASE-ENGINEER` understands that any task appropriate for a base
engineer is also appropriate for this agent — without the authority needing to
enumerate every inherited capability explicitly.

## Relation to the Ownership Manifest

The authority builder reads deployed files from `~/.claude/agents/`, not source
files from `~/.trusty-mpm/framework/agents/`. This matters for two reasons:

1. **What's deployed is what's authoritative.** A user who installs a new agent
   from a registry but has not yet run `trusty-mpm session start` to deploy it
   will not see that agent in the authority. The authority reflects reality, not
   intent.

2. **User-modified agents are included.** If a user has edited a deployed agent
   file (which trusty-mpm detected via checksum mismatch and skipped on the
   last deploy), the user's version is what gets scanned. The authority reflects
   the user's modifications, not the framework default.

## Relation to the Delegation Type

At runtime, the daemon tracks each act of delegation as a `Delegation` value
(defined in `trusty-mpm-core::agent`). A `Delegation` records the target agent
name, the model tier, the parent delegation (to reconstruct the delegation
tree), the task description, and the current lifecycle status. The authority
document produced here is the pre-session map; the runtime `Delegation` records
are the per-session trace.

See also:

- [Agent Inheritance System](./agent-inheritance.md) — how agent files are
  composed before the authority builder runs
- [Instruction Build Pipeline](./instruction-pipeline.md) — where the authority
  document fits in the overall instruction merge
