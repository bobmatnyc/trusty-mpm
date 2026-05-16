//! Well-known filesystem paths for the trusty-mpm framework installation.
//!
//! Why: the installer, the daemon, and the file watcher all need a single,
//! consistent answer for "where does the framework live?" — hard-coding
//! `~/.trusty-mpm/...` in three places invites drift.
//! What: [`FrameworkPaths`] resolves the framework directory layout rooted at
//! `~/.trusty-mpm`, plus convenience accessors for the two files the daemon
//! reads directly (the optimizer policy and the framework instructions).
//! Test: `cargo test -p trusty-mpm-core paths` asserts the resolved root
//! contains `.trusty-mpm` and that the subdirectories nest correctly.

use std::path::{Path, PathBuf};

/// Directory name (under the user's home) that holds the framework install.
pub const FRAMEWORK_DIR_NAME: &str = ".trusty-mpm";

/// Resolved paths for a trusty-mpm framework installation.
///
/// Why: groups every framework path behind one value so callers pass a single
/// `FrameworkPaths` instead of recomputing joins.
/// What: the install root and each artifact subdirectory; build with
/// [`FrameworkPaths::default`] (home-relative) or [`FrameworkPaths::under`]
/// (for tests against a temp dir).
/// Test: `default_resolves_under_trusty_mpm`, `under_nests_subdirectories`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameworkPaths {
    /// `~/.trusty-mpm`
    pub root: PathBuf,
    /// `~/.trusty-mpm/framework`
    pub framework: PathBuf,
    /// `~/.trusty-mpm/framework/agents`
    pub agents: PathBuf,
    /// `~/.trusty-mpm/framework/skills`
    pub skills: PathBuf,
    /// `~/.trusty-mpm/framework/hooks`
    pub hooks: PathBuf,
    /// `~/.trusty-mpm/framework/instructions`
    pub instructions: PathBuf,
    /// `~/.trusty-mpm/registry`
    pub registry: PathBuf,
    /// `~/.claude/agents` — where Claude Code reads composed agent files.
    pub claude_agents: PathBuf,
}

impl FrameworkPaths {
    /// Resolve the framework layout rooted at the user's home directory.
    ///
    /// Why: production callers want `~/.trusty-mpm` without each one resolving
    /// the home directory itself.
    /// What: locates the home directory via the `dirs` crate, falling back to
    /// the current directory if it cannot be determined (e.g. a stripped CI
    /// environment) so the type is always constructible.
    /// Test: `default_resolves_under_trusty_mpm`.
    #[allow(clippy::should_implement_trait)] // Intentional: no meaningful Default without I/O.
    pub fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        Self::under(home)
    }

    /// Resolve the framework layout under an arbitrary base directory.
    ///
    /// Why: tests must exercise install / reload logic without touching the
    /// real `~/.trusty-mpm`; pointing `base` at a `tempfile::TempDir` keeps
    /// them hermetic.
    /// What: joins `<base>/.trusty-mpm` and derives every subdirectory from it.
    /// Test: `under_nests_subdirectories`.
    pub fn under(base: impl AsRef<Path>) -> Self {
        let base = base.as_ref();
        let root = base.join(FRAMEWORK_DIR_NAME);
        let framework = root.join("framework");
        Self {
            agents: framework.join("agents"),
            skills: framework.join("skills"),
            hooks: framework.join("hooks"),
            instructions: framework.join("instructions"),
            registry: root.join("registry"),
            claude_agents: base.join(".claude").join("agents"),
            framework,
            root,
        }
    }

    /// Path of the token-optimizer policy file (`hooks/optimizer.toml`).
    ///
    /// Why: the daemon reads this at startup and on file-change to build its
    /// `OptimizerConfig`.
    /// What: `hooks/optimizer.toml` under the framework root.
    /// Test: `optimizer_config_path_is_under_hooks`.
    pub fn optimizer_config(&self) -> PathBuf {
        self.hooks.join("optimizer.toml")
    }

    /// Path of the session-overseer policy file (`hooks/overseer.toml`).
    ///
    /// Why: the daemon reads this at startup to build its `OverseerConfig`;
    /// keeping the path next to [`optimizer_config`](Self::optimizer_config)
    /// means both framework hook policies resolve consistently.
    /// What: `hooks/overseer.toml` under the framework root.
    /// Test: `overseer_config_path_is_under_hooks`.
    pub fn overseer_config(&self) -> PathBuf {
        self.hooks.join("overseer.toml")
    }

    /// Path of the framework launch instructions (`instructions/INSTRUCTIONS.md`).
    ///
    /// Why: launchers point new Claude Code sessions at this file; it is the
    /// framework artifact owned and overwritten by trusty-mpm on every install.
    /// What: `instructions/INSTRUCTIONS.md` under the framework root.
    /// Test: `instructions_path_is_under_instructions`.
    pub fn framework_instructions(&self) -> PathBuf {
        self.instructions.join("INSTRUCTIONS.md")
    }

    /// Path of the framework launch instructions — explicit-name alias.
    ///
    /// Why: the instruction merge pipeline refers to this file as
    /// `framework_instructions_path`; providing the alias keeps call sites
    /// readable without renaming the established [`framework_instructions`]
    /// accessor.
    /// What: delegates to [`framework_instructions`](Self::framework_instructions).
    /// Test: `framework_instructions_path_matches_accessor`.
    pub fn framework_instructions_path(&self) -> PathBuf {
        self.framework_instructions()
    }

    /// Path of the user-editable instruction stub (`instructions/CLAUDE.md`).
    ///
    /// Why: the installer seeds this stub once for project-specific notes;
    /// distinguishing it from `framework_instructions()` lets the installer
    /// avoid clobbering user edits on re-install.
    /// What: `instructions/CLAUDE.md` under the framework root.
    /// Test: `claude_stub_path_is_under_instructions`.
    pub fn claude_stub(&self) -> PathBuf {
        self.instructions.join("CLAUDE.md")
    }

    /// Directory holding the trusty-mpm agent *source* files.
    ///
    /// Why: the agent build pipeline reads `extends:`-bearing source agents
    /// from here and composes them before deployment.
    /// What: `framework/agents` under the framework root — the same directory
    /// `trusty-mpm install` writes the bundled agent sources into.
    /// Test: `agent_source_dir_is_framework_agents`.
    pub fn agent_source_dir(&self) -> PathBuf {
        self.agents.clone()
    }

    /// Directory Claude Code reads composed agent files from (`~/.claude/agents`).
    ///
    /// Why: the deploy step writes inheritance-flattened agents here so Claude
    /// Code sees self-contained files with no `extends:` to interpret.
    /// What: `.claude/agents` under the same base this `FrameworkPaths` was
    /// resolved against (the user's home for [`default`](Self::default), the
    /// temp dir for [`under`](Self::under)).
    /// Test: `claude_agents_dir_is_dotclaude_agents`.
    pub fn claude_agents_dir(&self) -> PathBuf {
        self.claude_agents.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_resolves_under_trusty_mpm() {
        // The home-relative resolver must always land inside a `.trusty-mpm`
        // directory regardless of which home directory the host reports.
        let paths = FrameworkPaths::default();
        assert!(
            paths.root.ends_with(FRAMEWORK_DIR_NAME),
            "root = {}",
            paths.root.display()
        );
        assert!(paths.framework.starts_with(&paths.root));
    }

    #[test]
    fn under_nests_subdirectories() {
        // Given an explicit base, every subdirectory must nest under the
        // framework root with the documented layout.
        let paths = FrameworkPaths::under("/base");
        assert_eq!(paths.root, PathBuf::from("/base/.trusty-mpm"));
        assert_eq!(
            paths.framework,
            PathBuf::from("/base/.trusty-mpm/framework")
        );
        assert_eq!(
            paths.agents,
            PathBuf::from("/base/.trusty-mpm/framework/agents")
        );
        assert_eq!(
            paths.skills,
            PathBuf::from("/base/.trusty-mpm/framework/skills")
        );
        assert_eq!(
            paths.hooks,
            PathBuf::from("/base/.trusty-mpm/framework/hooks")
        );
        assert_eq!(
            paths.instructions,
            PathBuf::from("/base/.trusty-mpm/framework/instructions")
        );
        assert_eq!(paths.registry, PathBuf::from("/base/.trusty-mpm/registry"));
    }

    #[test]
    fn optimizer_config_path_is_under_hooks() {
        let paths = FrameworkPaths::under("/base");
        assert_eq!(
            paths.optimizer_config(),
            PathBuf::from("/base/.trusty-mpm/framework/hooks/optimizer.toml")
        );
    }

    #[test]
    fn overseer_config_path_is_under_hooks() {
        let paths = FrameworkPaths::under("/base");
        assert_eq!(
            paths.overseer_config(),
            PathBuf::from("/base/.trusty-mpm/framework/hooks/overseer.toml")
        );
    }

    #[test]
    fn instructions_path_is_under_instructions() {
        let paths = FrameworkPaths::under("/base");
        assert_eq!(
            paths.framework_instructions(),
            PathBuf::from("/base/.trusty-mpm/framework/instructions/INSTRUCTIONS.md")
        );
    }

    #[test]
    fn framework_instructions_path_matches_accessor() {
        // The explicit-name alias must resolve identically to the original.
        let paths = FrameworkPaths::under("/base");
        assert_eq!(
            paths.framework_instructions_path(),
            paths.framework_instructions()
        );
    }

    #[test]
    fn claude_stub_path_is_under_instructions() {
        // The user stub lives alongside the framework instructions but under
        // the `CLAUDE.md` name Claude Code reads by convention.
        let paths = FrameworkPaths::under("/base");
        assert_eq!(
            paths.claude_stub(),
            PathBuf::from("/base/.trusty-mpm/framework/instructions/CLAUDE.md")
        );
    }

    #[test]
    fn agent_source_dir_is_framework_agents() {
        // Agent sources must resolve to `framework/agents` under the root.
        let paths = FrameworkPaths::under("/base");
        assert_eq!(
            paths.agent_source_dir(),
            PathBuf::from("/base/.trusty-mpm/framework/agents")
        );
    }

    #[test]
    fn claude_agents_dir_is_dotclaude_agents() {
        // Composed agents must deploy to `.claude/agents` under the base —
        // sibling to `.trusty-mpm`, not nested within it.
        let paths = FrameworkPaths::under("/base");
        assert_eq!(
            paths.claude_agents_dir(),
            PathBuf::from("/base/.claude/agents")
        );
    }

    #[test]
    fn framework_instructions_and_stub_are_distinct() {
        // The framework artifact and the user stub must never resolve to the
        // same path, or the installer would overwrite user edits.
        let paths = FrameworkPaths::under("/base");
        assert_ne!(paths.framework_instructions(), paths.claude_stub());
    }
}
