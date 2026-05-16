# Artifact Compatibility

trusty-mpm is **100% artifact compatible** with claude-mpm. Existing agent,
skill, and hook definitions work unchanged. This document defines the
compatibility contract and how artifacts are handled out-of-band (OOB).

## Compatibility contract

| Artifact | claude-mpm format | trusty-mpm |
|----------|-------------------|------------|
| Agent | `.md` with YAML frontmatter | byte-identical, parsed by `AgentArtifact::parse` |
| Skill | bundled `.md` / `SKILL.md` | byte-identical, parsed by `SkillArtifact` |
| Hook event names | the Claude Code event vocabulary | identical PascalCase wire names (`HookEvent`) |

### Hook event vocabulary — the full 32

claude-mpm wires only a handful of hook events by default
(`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `SubagentStop`,
`SessionStart`, `UserPromptSubmit`). trusty-mpm's `HookEvent` enum models **all
32** Claude Code lifecycle events so the universal hook relay can forward every
one of them — `PreCompact`, `PostCompact`, `WorktreeCreate`, `WorktreeRemove`,
`TeammateIdle`, `InstructionsLoaded`, `ConfigChange`, `CwdChanged`,
`FileChanged`, `TaskCreated`, `TaskCompleted`, `TaskUpdated`, `TaskStopped`,
`StopFailure`, `SubagentStopFailure`, `PermissionDenied`, `PermissionGranted`,
`Notification`, `McpServerConnected`, `McpServerDisconnected`, `SubagentStart`,
`TokenUsageUpdate`, `ErrorRaised`, `SkillActivated`, and `SessionEnd`.

serde uses the exact PascalCase Claude Code wire names, so `settings.json`
semantics and forwarded event JSON deserialize unchanged. `HookEvent::wire_name`
/ `HookEvent::from_wire` give a single, round-trip-tested conversion point, and
`HookEvent::category` groups the 32 into coarse `HookCategory` buckets (Tool /
Agent / Session / Memory / Worktree / File / Task / Permission / System) for
the dashboard panels and Telegram alert filters. This is a *superset* of
claude-mpm's vocabulary — every claude-mpm event name is present, plus the rest.

Unknown frontmatter keys are preserved (`AgentArtifact.extra` via
`#[serde(flatten)]`) so artifacts survive a load → serve round-trip without
loss. This is essential because claude-mpm frontmatter mixes two schemas:

- **MPM-proprietary**: `agent_id`, `agent_type`, `resource_tier`,
  `schema_version`, `capabilities`, `temperature`, `max_tokens`, `timeout`.
- **Claude Code native**: `name`, `description`, `model`, `tools`,
  `disallowedTools`, `permissionMode`, `maxTurns`, `memory`, `skills`,
  `hooks`, `background`, `effort`, `isolation`, `color`.

trusty-mpm-core models the fields it actively uses (`name`, `description`,
`model`, `skills`) and flattens the rest.

## Out-of-band handling

The core idea: **claude-mpm injects itself into Claude Code's config;
trusty-mpm intercepts from outside it.**

### Hooks — no settings.json injection

claude-mpm registers hook commands in `.claude/settings.json`:

```json
{ "hooks": { "PreToolUse": [{ "hooks": [{ "command": "claude-hook" }] }] } }
```

trusty-mpm's daemon owns the session process (tmux/PTY/SDK). It observes the
tool-use event stream directly and applies hook logic in-process. The daemon
**may still write a minimal settings.json shim** that points hooks at a thin
forwarder to the daemon socket — but the heavy logic (circuit breaker, model
tier, ztk) never spawns a process. Two modes:

1. **Forwarder mode** — settings.json hook = one tiny binary that forwards the
   event JSON to the daemon and relays the verdict. Compatible with stock
   Claude Code.
2. **Intercept mode** — when running under SDK/headless control, the daemon
   reads the event stream natively; no settings.json entry needed.

### Agents — served from the artifact store

The daemon maintains an artifact store indexed from:

- `~/.claude/agents/` (user)
- `<project>/.claude/agents/` (project)
- trusty-mpm's own bundled agent set

On session start the daemon materializes the relevant agent `.md` files into
the location Claude Code expects, OR (SDK mode) supplies them via the SDK's
agent configuration. Either way the **source of truth is the daemon**, not
scattered files.

### Skills — resolved before Claude Code sees the prompt

claude-mpm injects skills as slash commands. trusty-mpm's daemon intercepts
`UserPromptSubmit`, detects skill references (`/skill-name`), resolves the
skill body from the artifact store, and expands it into the prompt **before**
forwarding to Claude Code. The model never sees an unresolved slash command.

## Artifact store design

```
ArtifactStore
├── agents:  HashMap<String, AgentArtifact>   keyed by name
├── skills:  HashMap<String, SkillArtifact>   keyed by name
└── sources: Vec<PathBuf>                     watched directories
```

- Loaded on daemon start; refreshed on `SIGHUP` or a `reload` IPC request.
- Precedence: project artifacts override user artifacts override bundled.
- Validation at load time: malformed frontmatter → `Error::Artifact`, logged
  and skipped (one bad file does not break the store).

## Migration

A claude-mpm project needs **no artifact changes**. Migration is purely
operational (see `docs/migration` issues): install `trusty-mpmd`, point it at
the existing `.claude/` directory, optionally swap the settings.json hook
command for the trusty-mpm forwarder.

## claude-mpm Reference

| trusty-mpm Feature | claude-mpm Source | Notes |
|---|---|---|
| Agent `.md` frontmatter format (MPM-proprietary fields) | `src/claude_mpm/agents/frontmatter_validator.py`, `src/claude_mpm/agents/agents_metadata.py` | Validates `agent_id`, `agent_type`, `resource_tier`, `schema_version`, `capabilities`, `temperature`, `max_tokens`, `timeout` |
| Agent `.md` frontmatter format (Claude Code native fields) | `src/claude_mpm/migrations/v6_3_0_native_agent_fields.py` | Migrates agents to include `name`, `description`, `model`, `tools`, `disallowedTools`, `permissionMode`, `maxTurns`, `memory`, `skills`, `hooks`, `background`, `effort`, `isolation`, `color` |
| Bundled agent `.md` files | `src/claude_mpm/agents/bundled/` + `.claude/agents/*.md` | Source of agent definitions; `BASE_AGENT.md`, `BASE_ENGINEER.md`, `BASE_PM.md` are base templates |
| Skill `SKILL.md` format | `src/claude_mpm/skills/bundled/` | Bundled skills organized by domain (`pm/`, `collaboration/`); loaded by `skill_manager.py` and `skills_service.py` |
| Hook event names (`PreToolUse`, `PostToolUse`, etc.) | `.claude/settings.json` `"hooks"` key, `src/claude_mpm/hooks/claude_hooks/handlers/` | Event handlers: `tool_handler.py`, `stop_handler.py`, `user_prompt_handler.py`, `assistant_response_handler.py`, `subagent_handler.py` |
| Hook settings.json injection | `src/claude_mpm/services/hook_installer_service.py`, `src/claude_mpm/services/hook_service.py` | Writes hook commands into `.claude/settings.json`; trusty-mpm daemon optionally writes a thin forwarder shim instead |
| Artifact precedence (project > user > bundled) | `src/claude_mpm/services/agents/loading/`, `src/claude_mpm/agents/agent_loader.py` | Loads agents from project `.claude/agents/`, user `~/.claude/agents/`, then bundled |
| OOB skill resolution on `UserPromptSubmit` | `src/claude_mpm/hooks/claude_hooks/handlers/user_prompt_handler.py` | Intercepts user prompts to inject skill content before Claude Code sees them |
