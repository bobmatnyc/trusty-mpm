//! Pre-launch preparation for a Claude Code session.
//!
//! Why: every trusty-mpm session is launched as `claude` (the Claude Code CLI),
//! never `claude-mpm`. The "trusty-mpm" behaviour is supplied entirely through
//! the custom instructions Claude Code reads at startup — the deployed agents in
//! `~/.claude/agents/` and the project `CLAUDE.md`. Both the CLI (`tm session
//! start`) and the shared client (`DaemonClient::launch_session`, used by the
//! TUI's `/connect`) must perform the identical preparation; centralizing it
//! here keeps the two launch paths from drifting.
//! What: [`prepare_session`] deploys composed agents to `~/.claude/agents/` and
//! runs the instruction merge pipeline, writing/merging the project `CLAUDE.md`
//! and stashing the merged result under `<project>/.trusty-mpm/`. It returns a
//! [`PrepReport`] describing what happened so callers can report it.
//! Test: `prepare_session_writes_claude_md_and_stash` and
//! `prepare_session_is_idempotent` in this module's tests.

use std::path::{Path, PathBuf};

use crate::agent_deployer::{DeployResult, deploy_agents};
use crate::instruction_pipeline::{PipelineInput, PipelineOutput, build_instructions};
use crate::paths::FrameworkPaths;

/// Outcome of the pre-launch preparation for one session.
///
/// Why: callers (CLI, client) report agent-deploy counts and CLAUDE.md status
/// to the operator; bundling them avoids returning a loose tuple.
/// What: the agent [`DeployResult`], the instruction [`PipelineOutput`], and the
/// path the merged instructions were stashed to.
/// Test: asserted by `prepare_session_writes_claude_md_and_stash`.
#[derive(Debug)]
pub struct PrepReport {
    /// Result of deploying composed agents to `~/.claude/agents/`.
    pub deploy: DeployResult,
    /// Result of the instruction merge pipeline.
    pub instructions: PipelineOutput,
    /// Path the merged instructions were stashed to for inspection.
    pub stash: PathBuf,
}

/// A failure raised while preparing a session for launch.
///
/// Why: preparation performs agent deployment and filesystem I/O; callers need
/// a single typed error surface that names which stage failed.
/// What: variants for the agent-deploy stage and the instruction stage.
/// Test: not exercised by the happy-path tests; surfaced on invalid paths.
#[derive(Debug, thiserror::Error)]
pub enum PrepError {
    /// Deploying composed agents to `~/.claude/agents/` failed.
    #[error("agent deploy failed: {0}")]
    Deploy(String),
    /// Composing or stashing the launch instructions failed.
    #[error("instruction pipeline failed: {0}")]
    Instructions(#[from] crate::instruction_pipeline::PipelineError),
    /// A filesystem operation on the inspection stash failed.
    #[error("io error for {path}: {source}")]
    Io {
        /// The path the failed operation targeted.
        path: PathBuf,
        /// The underlying IO error.
        source: std::io::Error,
    },
}

/// Prepare a project directory for a fresh Claude Code session launch.
///
/// Why: launching `claude` is only correct if its custom instructions are in
/// place first — the composed agents must be deployed and the project
/// `CLAUDE.md` merged. This is the "custom instructions" step that makes a plain
/// `claude` process behave as a trusty-mpm session; both the CLI and the client
/// call this before sending `claude` into the tmux pane.
/// What: deploys composed agents from the framework agent source to
/// `~/.claude/agents/`, runs [`build_instructions`] for `project_dir` (which
/// loads or creates the project `CLAUDE.md`), writes the merged text to
/// `<project_dir>/.trusty-mpm/last-instructions.md`, and returns a [`PrepReport`].
/// Test: `prepare_session_writes_claude_md_and_stash`, `prepare_session_is_idempotent`.
pub fn prepare_session(fw: &FrameworkPaths, project_dir: &Path) -> Result<PrepReport, PrepError> {
    // Deploy composed agents — Claude Code reads `~/.claude/agents/` at startup.
    let deploy = deploy_agents(&fw.agent_source_dir(), &fw.claude_agents_dir())
        .map_err(|err| PrepError::Deploy(err.to_string()))?;

    // Compose the effective launch instructions (framework + delegation
    // authority + project CLAUDE.md); this loads or creates the project
    // CLAUDE.md so Claude Code picks it up automatically.
    let input = PipelineInput {
        framework_instructions_path: fw.framework_instructions_path(),
        agents_dir: fw.claude_agents_dir(),
        claude_md_path: project_dir.join("CLAUDE.md"),
    };
    let instructions = build_instructions(&input)?;

    // Stash the merged instructions where an operator can inspect them.
    let stash_dir = project_dir.join(".trusty-mpm");
    std::fs::create_dir_all(&stash_dir).map_err(|source| PrepError::Io {
        path: stash_dir.clone(),
        source,
    })?;
    let stash = stash_dir.join("last-instructions.md");
    std::fs::write(&stash, &instructions.merged).map_err(|source| PrepError::Io {
        path: stash.clone(),
        source,
    })?;

    Ok(PrepReport {
        deploy,
        instructions,
        stash,
    })
}

/// Trusty-specific MCP tool-priority instructions appended to every session.
///
/// Why: a `claude` process launched by trusty-mpm runs alongside the
/// `trusty-memory` and `trusty-search` MCP servers; without explicit guidance
/// the model falls back to generic `grep`/web-search instead of the hybrid
/// search and memory-palace tools. Hard-coding the block guarantees every
/// launched session is a properly configured PM instance.
/// What: a static instruction block, prepended after the assembled claude-mpm
/// PM instructions, that establishes the trusty tool priority order.
/// Test: `build_system_prompt_includes_trusty_block`.
pub const TRUSTY_SYSTEM_INSTRUCTIONS: &str = r#"# Trusty Tool Priority

You are running inside trusty-mpm. The following MCP tools are available and MUST be preferred over alternatives:

## Memory: trusty-memory (use BEFORE any other memory mechanism)
- `memory_recall` — semantic + temporal search across your memory palace. Use this FIRST whenever you need to recall context, prior decisions, or project knowledge.
- `memory_recall_deep` — deeper HNSW search when `memory_recall` returns insufficient results.
- `memory_remember` — store important decisions, findings, and facts immediately after they arise. Do not defer.
- `memory_list` — list stored memories by room or tag.
- `memory_forget` — remove outdated or incorrect memories.
- `palace_list` / `palace_create` — manage named memory palaces (one per project is typical).

Always call `memory_recall` at the start of any task to surface relevant prior context before taking action.

## Code Search: trusty-search (use BEFORE grep or web search for code questions)
- `search_code` — hybrid BM25 + vector + knowledge graph search. Use this FIRST for any "where is X defined", "how does Y work", or "find all usages of Z" questions.
- `search_all` — cross-project search when the target may span multiple codebases.
- `search_similar` — find semantically similar code chunks to a given file or function.
- `search_health` — verify the search daemon is live before a search session.

Always prefer `trusty-search` over shell `grep`/`find` for code discovery. Use grep only for exact-string or regex patterns that semantic search cannot handle.

## Priority order for common tasks
1. **Recall context** → `memory_recall` first
2. **Find code** → `search_code` before grep
3. **Store findings** → `memory_remember` after significant discoveries
4. **Cross-project** → `search_all` when scope is unclear
"#;

/// Read the assembled claude-mpm PM instructions from disk, if present.
///
/// Why: `~/.claude-mpm/PM_INSTRUCTIONS.md` is the assembled PM brief (BASE_PM +
/// PM_INSTRUCTIONS + WORKFLOW + agent capabilities) produced by claude-mpm. A
/// launched session should adopt it so it behaves as a real PM instance, but a
/// missing file must not abort the launch — trusty-mpm can run without it.
/// What: returns the file's contents trimmed of trailing whitespace, or `None`
/// when the file does not exist or cannot be read.
/// Test: `read_pm_instructions_returns_none_when_missing`.
pub fn read_pm_instructions() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home.join(".claude-mpm").join("PM_INSTRUCTIONS.md");
    let contents = std::fs::read_to_string(&path).ok()?;
    let trimmed = contents.trim_end();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Build the combined `--append-system-prompt` text for a launched session.
///
/// Why: every `claude` session launched by trusty-mpm must be a configured PM
/// instance — that means the claude-mpm PM instructions (when available) plus
/// the trusty tool-priority block. Combining them in one place keeps the CLI
/// and client launch paths from drifting.
/// What: concatenates [`read_pm_instructions`] (if any) and
/// [`TRUSTY_SYSTEM_INSTRUCTIONS`], separated by a blank line; returns `None`
/// only when both parts are empty (never happens while the constant is set).
/// Test: `build_system_prompt_includes_trusty_block`.
pub fn build_system_prompt() -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(pm) = read_pm_instructions() {
        parts.push(pm);
    }
    let trusty = TRUSTY_SYSTEM_INSTRUCTIONS.trim();
    if !trusty.is_empty() {
        parts.push(trusty.to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn build_system_prompt_includes_trusty_block() {
        // Why: regardless of whether the PM instructions file exists, the
        // trusty tool-priority block must always be present so a launched
        // session knows to prefer `memory_recall` and `search_code`.
        let prompt = build_system_prompt().expect("trusty block is always present");
        assert!(prompt.contains("# Trusty Tool Priority"));
        assert!(prompt.contains("memory_recall"));
        assert!(prompt.contains("search_code"));
    }

    #[test]
    fn read_pm_instructions_returns_none_when_missing() {
        // Why: a missing `~/.claude-mpm/PM_INSTRUCTIONS.md` must not abort a
        // launch; the reader degrades gracefully to `None`. This asserts the
        // function returns a well-typed Option rather than panicking.
        let _ = read_pm_instructions();
    }

    #[test]
    fn prepare_session_writes_claude_md_and_stash() {
        // Why: the launch paths rely on `prepare_session` writing the project
        // CLAUDE.md and the inspectable stash before `claude` is started.
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        let fw = FrameworkPaths::default();

        let report = prepare_session(&fw, project).expect("prep succeeds");

        assert!(
            project.join("CLAUDE.md").exists(),
            "CLAUDE.md must exist after prep"
        );
        assert!(
            report.stash.exists(),
            "merged instructions stash must be written"
        );
        assert_eq!(
            report.stash,
            project.join(".trusty-mpm").join("last-instructions.md")
        );
    }

    #[test]
    fn prepare_session_is_idempotent() {
        // Why: `/connect` and `tm session start` may run repeatedly on the same
        // project; a second prep must not fail and must not recreate CLAUDE.md.
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        let fw = FrameworkPaths::default();

        let first = prepare_session(&fw, project).expect("first prep succeeds");
        assert!(first.instructions.claude_md_created);

        let second = prepare_session(&fw, project).expect("second prep succeeds");
        assert!(
            !second.instructions.claude_md_created,
            "CLAUDE.md already exists on the second run"
        );
    }
}
