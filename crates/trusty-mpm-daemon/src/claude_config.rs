//! Claude Code configuration analyzer (I/O side).
//!
//! Why: `trusty-mpm-core::claude_config` defines the pure data model and path
//! resolution; the daemon owns the filesystem reads, the recommendation logic,
//! and the apply/restart actions. Keeping the I/O here preserves `core`'s
//! purity while still letting trusty-mpm inspect and improve a project's
//! Claude Code setup.
//! What: [`ClaudeConfigAnalyzer`] reads + merges the settings files, produces
//! [`ConfigRecommendation`]s, and applies them; [`ClaudeCodeRestarter`] finds
//! running `claude` processes and restarts Claude Code inside a tmux session.
//! Test: `cargo test -p trusty-mpm-daemon claude_config` covers reading,
//! analysis, and apply against temp directories (no real `~/.claude` touched).

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use trusty_mpm_core::claude_config::{
    ClaudeConfig, ClaudeConfigPaths, ConfigRecommendation, Severity,
};
use trusty_mpm_core::tmux::TmuxTarget;
use trusty_mpm_core::{Error, Result};

/// Reads, analyzes, and edits Claude Code configuration on disk.
///
/// Why: a unit type groups the I/O operations that act on a
/// [`ClaudeConfigPaths`]; none of them needs instance state.
/// What: `read_config` merges the four settings files into a [`ClaudeConfig`],
/// `analyze` turns that into recommendations, `apply_recommendation` writes the
/// fix back to disk.
/// Test: `read_config_detects_hooks`, `analyze_flags_missing_hooks`,
/// `apply_add_hooks_writes_settings`.
pub struct ClaudeConfigAnalyzer;

impl ClaudeConfigAnalyzer {
    /// Read and merge a project's Claude Code settings into a [`ClaudeConfig`].
    ///
    /// Why: recommendations are derived from a few high-level facts spread
    /// across four JSON files and two agent directories; merging them once
    /// keeps `analyze` simple.
    /// What: reads each settings file (missing files contribute nothing),
    /// OR-merges the `hooks` / `permissions.allow` / `env` facts, and scans the
    /// agent directories for `*.md` files. Never fails — an unreadable or
    /// malformed file is logged and skipped.
    /// Test: `read_config_detects_hooks`, `read_config_missing_files_is_empty`.
    pub fn read_config(paths: &ClaudeConfigPaths) -> ClaudeConfig {
        let mut config = ClaudeConfig::default();
        for settings_path in [
            &paths.user_settings,
            &paths.user_local_settings,
            &paths.project_settings,
            &paths.project_local_settings,
        ] {
            if let Some(json) = read_json(settings_path) {
                merge_settings(&mut config, &json);
            }
        }
        config.has_agents = dir_has_agent_files(&paths.user_agents_dir)
            || dir_has_agent_files(&paths.project_agents_dir);
        config
    }

    /// Produce config recommendations for an analyzed [`ClaudeConfig`].
    ///
    /// Why: trusty-mpm proactively surfaces config gaps — missing oversight
    /// hooks, an overly broad permission allow list, no deployed agents, a
    /// missing API key — so the operator can act on them.
    /// What: returns one [`ConfigRecommendation`] per detected issue; an
    /// already-healthy config yields an empty list.
    /// Test: `analyze_flags_missing_hooks`, `analyze_flags_wildcard`,
    /// `analyze_clean_config_is_empty`.
    pub fn analyze(config: &ClaudeConfig) -> Vec<ConfigRecommendation> {
        let mut recs = Vec::new();

        if !config.has_hooks {
            recs.push(ConfigRecommendation {
                id: "add-trusty-hooks".into(),
                severity: Severity::Warning,
                title: "No hooks configured".into(),
                description: "Claude Code has no hooks. Add pre/post tool-use \
hooks so trusty-mpm can observe and oversee tool calls."
                    .into(),
                auto_applicable: true,
            });
        }

        if config.allow_list_has_wildcard {
            recs.push(ConfigRecommendation {
                id: "scope-permissions".into(),
                severity: Severity::Critical,
                title: "Permission allow list contains a wildcard".into(),
                description: "The `permissions.allow` list contains `*`, which \
grants every tool unconditionally. Scope it to the specific tools the project \
needs."
                    .into(),
                auto_applicable: false,
            });
        }

        if !config.has_agents {
            recs.push(ConfigRecommendation {
                id: "deploy-agents".into(),
                severity: Severity::Info,
                title: "No agents deployed".into(),
                description: "No agent files were found. Deploy the trusty-mpm \
agents so delegated work runs under managed agents."
                    .into(),
                auto_applicable: false,
            });
        }

        if !config.has_openrouter_key {
            recs.push(ConfigRecommendation {
                id: "add-openrouter-key".into(),
                severity: Severity::Info,
                title: "OPENROUTER_API_KEY not in env hooks".into(),
                description: "The LLM overseer needs `OPENROUTER_API_KEY`. Add \
it to the Claude Code `env` block (or to `.env.local`)."
                    .into(),
                auto_applicable: false,
            });
        }

        recs
    }

    /// Apply a single recommendation, writing the fix to disk.
    ///
    /// Why: lets `POST /claude-config/apply` act on a recommendation without
    /// the operator hand-editing JSON.
    /// What: dispatches on `rec.id`. Only `add-trusty-hooks` is auto-applicable
    /// — it writes a minimal `hooks` block into the project `settings.json`.
    /// Recommendations that are not auto-applicable return an error explaining
    /// they need a manual fix.
    /// Test: `apply_add_hooks_writes_settings`, `apply_manual_rec_errors`.
    pub fn apply_recommendation(
        rec: &ConfigRecommendation,
        paths: &ClaudeConfigPaths,
    ) -> Result<()> {
        match rec.id.as_str() {
            "add-trusty-hooks" => apply_add_hooks(&paths.project_settings),
            other => Err(Error::Protocol(format!(
                "recommendation `{other}` is not auto-applicable; apply it manually"
            ))),
        }
    }
}

/// Read a JSON file, returning `None` when absent or malformed.
///
/// Why: settings files are optional and operator-edited; a missing or broken
/// file must never abort analysis.
/// What: reads `path`, parses it as JSON; logs and returns `None` on any error.
/// Test: `read_config_missing_files_is_empty` (missing path → `None`).
fn read_json(path: &Path) -> Option<Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str(&raw) {
        Ok(json) => Some(json),
        Err(e) => {
            tracing::warn!("malformed Claude config {}: {e}; skipping", path.display());
            None
        }
    }
}

/// OR-merge one settings JSON document's facts into a [`ClaudeConfig`].
///
/// Why: settings are layered (user → user.local → project → project.local);
/// the analyzer cares only whether *any* layer sets a fact, so booleans are
/// OR-merged and the allow-list count is summed.
/// What: sets `has_hooks` if the doc has a non-empty `hooks` object, scans
/// `permissions.allow` for a `*` and counts its entries, and checks the `env`
/// block for `OPENROUTER_API_KEY`.
/// Test: `read_config_detects_hooks`, `analyze_flags_wildcard`.
fn merge_settings(config: &mut ClaudeConfig, json: &Value) {
    if let Some(hooks) = json.get("hooks").and_then(Value::as_object)
        && !hooks.is_empty()
    {
        config.has_hooks = true;
    }
    if let Some(allow) = json
        .get("permissions")
        .and_then(|p| p.get("allow"))
        .and_then(Value::as_array)
    {
        config.allow_list_entries += allow.len();
        if allow.iter().any(|v| v.as_str() == Some("*")) {
            config.allow_list_has_wildcard = true;
        }
    }
    if let Some(env) = json.get("env").and_then(Value::as_object)
        && env.contains_key("OPENROUTER_API_KEY")
    {
        config.has_openrouter_key = true;
    }
}

/// True when `dir` exists and contains at least one `*.md` agent file.
///
/// Why: an agents directory may exist but be empty; the recommendation cares
/// about actual agent files.
/// What: scans `dir` for a directory entry with a `.md` extension.
/// Test: `read_config_detects_agents`.
fn dir_has_agent_files(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|e| {
        e.path()
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("md"))
    })
}

/// Write a minimal trusty-mpm `hooks` block into a project `settings.json`.
///
/// Why: the `add-trusty-hooks` recommendation is auto-applicable; this is its
/// effect.
/// What: reads the existing `settings.json` (or starts from `{}`), inserts a
/// `hooks` object covering `PreToolUse` / `PostToolUse` / `Stop`, creates the
/// `.claude` directory if needed, and writes the file back pretty-printed.
/// Test: `apply_add_hooks_writes_settings`.
fn apply_add_hooks(settings_path: &Path) -> Result<()> {
    let mut json: Value = read_json(settings_path).unwrap_or_else(|| serde_json::json!({}));
    let hooks = serde_json::json!({
        "PreToolUse": [{ "matcher": "*", "hooks": [
            { "type": "command", "command": "trusty-mpm hook" }
        ] }],
        "PostToolUse": [{ "matcher": "*", "hooks": [
            { "type": "command", "command": "trusty-mpm hook" }
        ] }],
        "Stop": [{ "matcher": "*", "hooks": [
            { "type": "command", "command": "trusty-mpm hook" }
        ] }],
    });
    if let Some(obj) = json.as_object_mut() {
        obj.insert("hooks".to_string(), hooks);
    } else {
        json = serde_json::json!({ "hooks": hooks });
    }
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let pretty = serde_json::to_string_pretty(&json)
        .map_err(|e| Error::Protocol(format!("serialize settings.json: {e}")))?;
    std::fs::write(settings_path, pretty).map_err(Error::Io)?;
    Ok(())
}

/// Finds and restarts running Claude Code processes.
///
/// Why: after applying config changes the operator wants Claude Code to pick
/// them up; this drives the restart.
/// What: `find_claude_processes` lists `claude` PIDs via `pgrep`;
/// `restart_in_session` sends Ctrl-C then `claude` into a tmux session's pane.
/// Test: `find_claude_processes_does_not_panic` (the PID list may be empty).
pub struct ClaudeCodeRestarter;

impl ClaudeCodeRestarter {
    /// List the PIDs of running `claude` processes.
    ///
    /// Why: the dashboard shows whether Claude Code is running and how many
    /// instances; the restart flow can also use it to confirm a target exists.
    /// What: runs `pgrep -x claude`; a non-zero exit (no matches) or a missing
    /// `pgrep` both yield an empty `Vec` rather than an error.
    /// Test: `find_claude_processes_does_not_panic`.
    pub fn find_claude_processes() -> Vec<u32> {
        let output = match Command::new("pgrep").args(["-x", "claude"]).output() {
            Ok(out) => out,
            Err(e) => {
                tracing::info!("pgrep unavailable: {e}; reporting no claude processes");
                return Vec::new();
            }
        };
        if !output.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .collect()
    }

    /// Restart Claude Code inside a named tmux session.
    ///
    /// Why: a Claude Code session hosted in tmux is restarted in place — send
    /// an interrupt to stop the current process, then relaunch `claude`.
    /// What: discovers tmux, sends `C-c` to the session's pane, waits briefly
    /// for the process to exit, then types `claude` + Enter. tmux being absent
    /// surfaces as an `Err`.
    /// Test: `restart_in_session_errors_without_tmux` (skipped when tmux is
    /// installed).
    pub fn restart_in_session(tmux_session: &str) -> Result<()> {
        let driver = crate::tmux::TmuxDriver::discover()?;
        let target = TmuxTarget::session(tmux_session);
        // Interrupt the running Claude Code process.
        driver.send_interrupt(&target)?;
        std::thread::sleep(std::time::Duration::from_millis(500));
        // Relaunch Claude Code.
        driver.send_line(&target, "claude")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trusty_mpm_core::claude_config::ClaudeConfigReader;

    /// Build a `ClaudeConfigPaths` rooted entirely under a temp directory so a
    /// test never reads or writes the operator's real `~/.claude`.
    fn temp_paths(root: &Path) -> ClaudeConfigPaths {
        let project = root.join("project");
        let user = root.join("home");
        ClaudeConfigPaths {
            user_settings: user.join(".claude/settings.json"),
            user_local_settings: user.join(".claude/settings.local.json"),
            project_settings: project.join(".claude/settings.json"),
            project_local_settings: project.join(".claude/settings.local.json"),
            user_agents_dir: user.join(".claude/agents"),
            project_agents_dir: project.join(".claude/agents"),
        }
    }

    fn write_json(path: &Path, json: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_string_pretty(json).unwrap()).unwrap();
    }

    #[test]
    fn read_config_missing_files_is_empty() {
        // No settings files on disk → an all-default ClaudeConfig.
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        let config = ClaudeConfigAnalyzer::read_config(&paths);
        assert_eq!(config, ClaudeConfig::default());
    }

    #[test]
    fn read_config_detects_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        write_json(
            &paths.project_settings,
            &serde_json::json!({ "hooks": { "PreToolUse": [] } }),
        );
        let config = ClaudeConfigAnalyzer::read_config(&paths);
        assert!(config.has_hooks);
    }

    #[test]
    fn read_config_detects_wildcard_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        write_json(
            &paths.project_settings,
            &serde_json::json!({
                "permissions": { "allow": ["*", "Read"] },
                "env": { "OPENROUTER_API_KEY": "sk-x" } // pragma: allowlist secret
            }),
        );
        let config = ClaudeConfigAnalyzer::read_config(&paths);
        assert!(config.allow_list_has_wildcard);
        assert_eq!(config.allow_list_entries, 2);
        assert!(config.has_openrouter_key);
    }

    #[test]
    fn read_config_detects_agents() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        std::fs::create_dir_all(&paths.project_agents_dir).unwrap();
        std::fs::write(paths.project_agents_dir.join("research.md"), "# agent").unwrap();
        let config = ClaudeConfigAnalyzer::read_config(&paths);
        assert!(config.has_agents);
    }

    #[test]
    fn analyze_flags_missing_hooks() {
        // A default (empty) config triggers the add-trusty-hooks recommendation.
        let recs = ClaudeConfigAnalyzer::analyze(&ClaudeConfig::default());
        assert!(recs.iter().any(|r| r.id == "add-trusty-hooks"));
    }

    #[test]
    fn analyze_flags_wildcard() {
        let config = ClaudeConfig {
            has_hooks: true,
            allow_list_has_wildcard: true,
            allow_list_entries: 1,
            has_agents: true,
            has_openrouter_key: true,
        };
        let recs = ClaudeConfigAnalyzer::analyze(&config);
        let wildcard = recs.iter().find(|r| r.id == "scope-permissions");
        assert!(wildcard.is_some());
        assert_eq!(wildcard.unwrap().severity, Severity::Critical);
    }

    #[test]
    fn analyze_clean_config_is_empty() {
        // A fully-configured project yields no recommendations.
        let config = ClaudeConfig {
            has_hooks: true,
            allow_list_has_wildcard: false,
            allow_list_entries: 5,
            has_agents: true,
            has_openrouter_key: true,
        };
        assert!(ClaudeConfigAnalyzer::analyze(&config).is_empty());
    }

    #[test]
    fn apply_add_hooks_writes_settings() {
        // Applying add-trusty-hooks must write a hooks block that a subsequent
        // read picks up as `has_hooks = true`.
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        let rec = &ClaudeConfigAnalyzer::analyze(&ClaudeConfig::default())[0];
        assert_eq!(rec.id, "add-trusty-hooks");
        ClaudeConfigAnalyzer::apply_recommendation(rec, &paths).expect("apply succeeds");

        let config = ClaudeConfigAnalyzer::read_config(&paths);
        assert!(config.has_hooks, "hooks block must be present after apply");
    }

    #[test]
    fn apply_manual_rec_errors() {
        // A non-auto-applicable recommendation cannot be applied programmatically.
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        let rec = ConfigRecommendation {
            id: "scope-permissions".into(),
            severity: Severity::Critical,
            title: "x".into(),
            description: "x".into(),
            auto_applicable: false,
        };
        assert!(ClaudeConfigAnalyzer::apply_recommendation(&rec, &paths).is_err());
    }

    #[test]
    fn find_claude_processes_does_not_panic() {
        // Whether or not Claude Code is running, this returns a Vec without
        // panicking — the count is environment-dependent.
        let _pids = ClaudeCodeRestarter::find_claude_processes();
    }

    #[test]
    fn paths_for_project_is_usable() {
        // The core resolver and the analyzer agree on the path shape.
        let paths = ClaudeConfigReader::paths_for_project(Path::new("/work/demo"));
        assert!(paths.project_settings.ends_with(".claude/settings.json"));
    }
}
