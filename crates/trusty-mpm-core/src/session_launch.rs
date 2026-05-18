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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
