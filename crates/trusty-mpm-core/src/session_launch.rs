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
    /// Path the `trusty-mpm` output style was deployed to, if it succeeded.
    ///
    /// `None` when deployment was skipped (no home directory) or failed; the
    /// session still launches in that case, just with the operator's default
    /// style.
    pub output_style: Option<PathBuf>,
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

    // Set the Claude Code output style so the launched session's status bar
    // reads `style:trusty-mpm`. A failure here is non-fatal: the session still
    // launches, it just shows the operator's default style.
    if let Err(err) = write_output_style(project_dir) {
        tracing::warn!("failed to set trusty-mpm output style: {err}");
    }

    // Deploy the bundled output-style definition so Claude Code can resolve the
    // `trusty-mpm` name written into `.claude/settings.json` above. Non-fatal:
    // a missing style file just falls back to the operator's default.
    let output_style = match dirs::home_dir() {
        Some(home) => match deploy_output_style(&home) {
            Ok(path) => Some(path),
            Err(err) => {
                tracing::warn!("failed to deploy trusty-mpm output style file: {err}");
                None
            }
        },
        None => {
            tracing::warn!("skipping output style deploy: home directory unresolved");
            None
        }
    };

    Ok(PrepReport {
        deploy,
        instructions,
        stash,
        output_style,
    })
}

/// Deploy the bundled `trusty-mpm` output style under `<home>/.claude/output-styles/`.
///
/// Why: [`write_output_style`] only sets `"outputStyle": "trusty-mpm"` in the
/// project settings; Claude Code honours that name only when a matching style
/// file exists in `~/.claude/output-styles/`. This places that file. `home` is
/// passed in (rather than resolved here) so tests can target a temp directory
/// instead of the operator's real home.
/// What: creates `<home>/.claude/output-styles/` if absent, then writes the
/// bundled [`crate::bundle::OUTPUT_STYLE`] asset, always overwriting so
/// framework upgrades to the style propagate on the next launch. Returns the
/// path written.
/// Test: `deploy_output_style_writes_file`, `deploy_output_style_overwrites`.
fn deploy_output_style(home: &Path) -> Result<PathBuf, PrepError> {
    let style_dir = home.join(".claude").join("output-styles");
    std::fs::create_dir_all(&style_dir).map_err(|source| PrepError::Io {
        path: style_dir.clone(),
        source,
    })?;
    let style_path = style_dir.join("trusty-mpm.md");
    std::fs::write(&style_path, crate::bundle::OUTPUT_STYLE).map_err(|source| PrepError::Io {
        path: style_path.clone(),
        source,
    })?;
    Ok(style_path)
}

/// Claude Code output style applied to launched sessions.
///
/// Why: the Claude Code status bar renders `style:<outputStyle>`; launched
/// trusty-mpm sessions should advertise themselves as `trusty-mpm`.
const OUTPUT_STYLE: &str = "trusty-mpm";

/// Merge `"outputStyle": "trusty-mpm"` into the project's `.claude/settings.json`.
///
/// Why: Claude Code reads the output style from `.claude/settings.json` under
/// the `outputStyle` key (there is no `--style` CLI flag); writing it in the
/// project directory makes every `claude` launched there show
/// `style:trusty-mpm` without disturbing the operator's global settings.
/// What: reads an existing `<project>/.claude/settings.json` (preserving all
/// other keys), sets `outputStyle` to [`OUTPUT_STYLE`], and writes it back
/// pretty-printed. Creates the file and `.claude/` directory when absent.
/// Test: `prepare_session_sets_output_style`,
/// `write_output_style_preserves_existing_keys`.
fn write_output_style(project_dir: &Path) -> Result<(), PrepError> {
    let claude_dir = project_dir.join(".claude");
    std::fs::create_dir_all(&claude_dir).map_err(|source| PrepError::Io {
        path: claude_dir.clone(),
        source,
    })?;
    let settings_path = claude_dir.join("settings.json");

    // Load existing settings to preserve unrelated keys; tolerate a missing or
    // malformed file by starting from an empty object.
    let mut settings = match std::fs::read_to_string(&settings_path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new())),
        Err(_) => serde_json::Value::Object(serde_json::Map::new()),
    };

    settings["outputStyle"] = serde_json::Value::String(OUTPUT_STYLE.to_string());

    let serialized = serde_json::to_string_pretty(&settings)
        .map_err(|err| PrepError::Deploy(err.to_string()))?;
    std::fs::write(&settings_path, serialized).map_err(|source| PrepError::Io {
        path: settings_path.clone(),
        source,
    })?;
    Ok(())
}

/// Build the `--append-system-prompt` text for a launched session.
///
/// Why: every `claude` session launched by trusty-mpm must be a configured PM
/// instance. trusty-mpm owns its PM instructions: they are assembled from
/// bundled assets into `~/.trusty-mpm/framework/instructions/INSTRUCTIONS.md`
/// and passed to `claude --append-system-prompt-file`.
/// What: reads `~/.trusty-mpm/framework/instructions/INSTRUCTIONS.md`; if it is
/// missing or empty (first run) it calls
/// [`crate::instruction_pipeline::install_system_prompt`] to generate it from
/// the bundled assets, then reads it back. Returns `None` only when the home
/// directory cannot be resolved or the file cannot be written/read.
/// Test: `build_system_prompt_includes_trusty_block`.
pub fn build_system_prompt() -> Option<String> {
    let home = dirs::home_dir()?;
    let path = home
        .join(".trusty-mpm")
        .join("framework")
        .join("instructions")
        .join("INSTRUCTIONS.md");

    // Use the on-disk file when it is present and non-empty.
    if let Ok(contents) = std::fs::read_to_string(&path) {
        let trimmed = contents.trim_end();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }

    // First run (or empty file): generate it from the bundled assets, then
    // read it back so the launch path always uses the same source of truth.
    let generated = crate::instruction_pipeline::install_system_prompt().ok()?;
    let contents = std::fs::read_to_string(&generated).ok()?;
    let trimmed = contents.trim_end();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn build_system_prompt_includes_trusty_block() {
        // Why: `build_system_prompt` must always yield a prompt — generating
        // `INSTRUCTIONS.md` from the bundled assets on first run — and that
        // prompt must include the trusty tool-priority block so a launched
        // session knows to prefer `memory_recall` and `search_code`.
        let prompt = build_system_prompt().expect("trusty block is always present");
        assert!(prompt.contains("# Trusty Tool Priority"));
        assert!(prompt.contains("memory_recall"));
        assert!(prompt.contains("search_code"));
        // The bundled PM instructions are also part of the assembled prompt.
        assert!(prompt.contains("# PM Agent -- Claude MPM"));
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
    fn prepare_session_sets_output_style() {
        // Why: a launched session must show `style:trusty-mpm`, which Claude
        // Code reads from `<project>/.claude/settings.json`.
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        let fw = FrameworkPaths::default();

        prepare_session(&fw, project).expect("prep succeeds");

        let settings_path = project.join(".claude").join("settings.json");
        assert!(settings_path.exists(), ".claude/settings.json must exist");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(value["outputStyle"], serde_json::json!("trusty-mpm"));
    }

    #[test]
    fn write_output_style_preserves_existing_keys() {
        // Why: merging the style must not clobber an operator's other settings.
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        let claude_dir = project.join(".claude");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::write(
            claude_dir.join("settings.json"),
            r#"{"theme":"dark","outputStyle":"old"}"#,
        )
        .unwrap();

        write_output_style(project).expect("write succeeds");

        let value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(claude_dir.join("settings.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(value["outputStyle"], serde_json::json!("trusty-mpm"));
        assert_eq!(value["theme"], serde_json::json!("dark"));
    }

    #[test]
    fn deploy_output_style_writes_file() {
        // Why: Claude Code resolves the `trusty-mpm` output style only when a
        // matching file exists in `~/.claude/output-styles/`; deployment must
        // create that file (and its parent dir) with the bundled content.
        let home = tempdir().unwrap();
        let path = deploy_output_style(home.path()).expect("deploy succeeds");

        assert_eq!(
            path,
            home.path()
                .join(".claude")
                .join("output-styles")
                .join("trusty-mpm.md")
        );
        let written = std::fs::read_to_string(&path).expect("style file readable");
        assert_eq!(written, crate::bundle::OUTPUT_STYLE);
        assert!(written.contains("name: trusty-mpm"));
    }

    #[test]
    fn deploy_output_style_overwrites() {
        // Why: framework upgrades to the style must propagate on the next
        // launch, so deployment always overwrites any existing file.
        let home = tempdir().unwrap();
        let first = deploy_output_style(home.path()).expect("first deploy succeeds");
        std::fs::write(&first, "stale operator content").unwrap();

        let second = deploy_output_style(home.path()).expect("second deploy succeeds");
        assert_eq!(first, second);
        let written = std::fs::read_to_string(&second).unwrap();
        assert_eq!(written, crate::bundle::OUTPUT_STYLE);
    }

    #[test]
    fn prepare_session_reports_output_style() {
        // Why: callers report the deployed style path; `prepare_session` must
        // populate `PrepReport.output_style` with the file it deployed.
        let tmp = tempdir().unwrap();
        let project = tmp.path();
        let fw = FrameworkPaths::default();

        let report = prepare_session(&fw, project).expect("prep succeeds");

        let style = report
            .output_style
            .expect("output style deployed when home is resolvable");
        assert!(style.ends_with("trusty-mpm.md"));
        assert!(style.exists());
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
