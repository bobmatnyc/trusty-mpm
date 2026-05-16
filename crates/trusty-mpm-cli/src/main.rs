//! trusty-mpm — the single unified binary.
//!
//! Why: `cargo install trusty-mpm` should install exactly one binary that
//! covers every mode — the resident daemon, the MCP server, the ratatui
//! dashboard, the Telegram bot, and the thin HTTP CLI. One binary keeps the
//! install story simple and the modes discoverable via `--help`.
//! What: parses subcommands and routes to the embedded library crates —
//! `trusty_mpm_daemon`, `trusty_mpm_tui`, `trusty_mpm_telegram` — or, for the
//! thin CLI subcommands, drives the daemon's HTTP API with an async `reqwest`
//! client.
//! Test: `cargo run -p trusty-mpm-cli -- status` prints daemon/session state;
//! handler and parsing logic are covered by `cargo test -p trusty-mpm-cli`.

use std::net::SocketAddr;

use clap::{Parser, Subcommand};
use serde::Deserialize;

/// Default daemon address when `--url` / `TRUSTY_MPM_URL` is unset.
const DEFAULT_URL: &str = "http://127.0.0.1:7880";

/// trusty-mpm command-line interface.
#[derive(Debug, Parser)]
#[command(name = "trusty-mpm", version, about = "trusty-mpm — unified binary")]
struct Cli {
    /// Base URL of the trusty-mpm daemon (used by the thin CLI subcommands).
    #[arg(long, env = "TRUSTY_MPM_URL", default_value = DEFAULT_URL, global = true)]
    url: String,

    /// Subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// Top-level CLI subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Show daemon and session status.
    Status,
    /// Define and manage projects (registered working directories).
    Project {
        /// Project action to perform.
        #[command(subcommand)]
        action: ProjectAction,
    },
    /// Define and manage Claude Code sessions within a project.
    Session {
        /// Session action to perform.
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Show the recent hook-event feed.
    Events,
    /// Launch the ratatui multi-session TUI dashboard.
    Tui {
        /// Base URL of the trusty-mpm daemon.
        #[arg(long, env = "TRUSTY_MPM_URL", default_value = DEFAULT_URL)]
        url: String,
        /// Poll interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
    },
    /// Run the Telegram remote-management bot.
    Telegram {
        /// Base URL of the trusty-mpm daemon.
        #[arg(long, env = "TRUSTY_MPM_URL", default_value = DEFAULT_URL)]
        url: String,
        /// Telegram bot token (read from the environment in production).
        #[arg(long, env = "TELEGRAM_BOT_TOKEN")]
        token: Option<String>,
        /// Validate configuration and exit without connecting to Telegram.
        #[arg(long)]
        check: bool,
    },
    /// Install the bundled framework artifacts to `~/.trusty-mpm/framework/`.
    Install {
        /// Overwrite artifacts that already exist on disk.
        #[arg(long)]
        force: bool,
    },
    /// Run the trusty-mpm daemon.
    Daemon {
        /// Address the daemon HTTP API binds to.
        #[arg(long, env = "TRUSTY_MPM_ADDR", default_value = "127.0.0.1:7880")]
        addr: SocketAddr,
        /// Run as an MCP server over stdio instead of the HTTP daemon.
        #[arg(long)]
        mcp: bool,
    },
    /// Inspect or configure the token-use optimizer.
    Optimizer {
        /// Optimizer action to perform.
        #[command(subcommand)]
        action: OptimizerAction,
    },
}

/// Actions for the `project` subcommand.
#[derive(Debug, Subcommand)]
enum ProjectAction {
    /// Register a working directory as a trusty-mpm project.
    Init {
        /// Directory to register (defaults to the cwd).
        #[arg(long)]
        dir: Option<String>,
    },
    /// List all registered projects with their status.
    List,
    /// Show the current project's registered info and config.
    Info {
        /// Project directory (defaults to the cwd).
        #[arg(long)]
        dir: Option<String>,
    },
}

/// Actions for the `session` subcommand.
#[derive(Debug, Subcommand)]
enum SessionAction {
    /// Start a new Claude Code session in the current/specified project.
    Start {
        /// Project directory for the new session (defaults to the cwd).
        #[arg(long)]
        dir: Option<String>,
    },
    /// Stop a session by id or friendly name.
    Stop {
        /// Session id or friendly name (e.g. `tmpm-quiet-falcon`).
        id_or_name: String,
    },
    /// List sessions for the current project.
    List {
        /// Project directory (defaults to the cwd).
        #[arg(long)]
        dir: Option<String>,
    },
    /// Reap dead sessions for the current project.
    Clean {
        /// Project directory (defaults to the cwd).
        #[arg(long)]
        dir: Option<String>,
    },
    /// Show detailed info for a specific session.
    Info {
        /// Session id or friendly name.
        id_or_name: String,
    },
}

/// Actions for the `optimizer` subcommand.
#[derive(Debug, Subcommand)]
enum OptimizerAction {
    /// Show current optimizer configuration.
    Status,
    /// Set the default compression level (rewrites the framework policy file).
    Set {
        /// Compression level: off, trim, summarise, caveman.
        #[arg(value_enum)]
        level: CliCompressionLevel,
    },
}

/// CLI-friendly compression level (mirrors `CompressionLevel`).
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum CliCompressionLevel {
    /// No compression.
    Off,
    /// Trim large outputs.
    Trim,
    /// Trim + strip ANSI + collapse blanks.
    Summarise,
    /// Drop all content, keep a one-line summary.
    Caveman,
}

/// One session row as returned by `GET /sessions`.
#[derive(Debug, Deserialize)]
struct SessionRow {
    /// Session id (a `SessionId` newtype: `{"0": "<uuid>"}`).
    id: serde_json::Value,
    /// Working directory.
    workdir: String,
    /// Lifecycle status string.
    status: serde_json::Value,
    /// Number of active delegations.
    #[serde(default)]
    active_delegations: u32,
}

/// One project row as returned by `GET /projects`.
#[derive(Debug, Deserialize)]
struct ProjectRow {
    /// Absolute project path.
    path: std::path::PathBuf,
    /// Human-readable project name.
    name: String,
}

/// One event row as returned by `GET /events`.
#[derive(Debug, Deserialize)]
struct EventRow {
    /// Originating session (`SessionId` newtype JSON).
    session: serde_json::Value,
    /// Claude Code wire event name.
    event: String,
    /// RFC3339 timestamp the daemon received the event.
    at: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Long-running modes need tracing on stderr (the daemon's MCP mode speaks
    // JSON-RPC on stdout, so all logs must stay off stdout).
    if matches!(cli.command, Command::Daemon { .. }) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .with_writer(std::io::stderr)
            .init();
    }

    let client = reqwest::Client::new();
    match cli.command {
        Command::Status => status(&client, &cli.url).await,
        Command::Project { action } => project(&client, &cli.url, action).await,
        Command::Session { action } => session(&client, &cli.url, action).await,
        Command::Events => events(&client, &cli.url).await,
        Command::Tui { url, interval_ms } => trusty_mpm_tui::run(url, interval_ms).await,
        Command::Telegram { url, token, check } => {
            trusty_mpm_telegram::run(url, token, check).await
        }
        Command::Install { force } => install(force),
        Command::Daemon { addr, mcp } => {
            let state = trusty_mpm_daemon::DaemonState::shared();
            if mcp {
                trusty_mpm_daemon::run_mcp(state).await
            } else {
                trusty_mpm_daemon::run_http(state, addr).await
            }
        }
        Command::Optimizer { action } => optimizer(&client, &cli.url, action).await,
    }
}

/// `install` subcommand — deploy the bundled framework artifacts.
///
/// Why: a fresh machine has no `~/.trusty-mpm/framework/`; `trusty-mpm install`
/// writes the compile-time-embedded artifacts (optimizer policy, framework
/// instructions, placeholder agent/skill) so the daemon has a working policy
/// and launchers have instructions to point sessions at.
/// What: resolves [`FrameworkPaths::default`] and delegates to
/// [`install_to`], which is the testable core.
/// Test: `install_writes_all_artifacts`, `install_skips_existing_without_force`.
fn install(force: bool) -> anyhow::Result<()> {
    let paths = trusty_mpm_core::paths::FrameworkPaths::default();
    let report = install_to(&paths, force)?;
    println!(
        "Installing trusty-mpm framework artifacts to {}",
        paths.framework.display()
    );
    for line in &report {
        println!("  {line}");
    }
    println!(
        "Composing agents into {}",
        paths.claude_agents_dir().display()
    );
    let deploy = trusty_mpm_core::agent_deployer::deploy_agents(
        &paths.agent_source_dir(),
        &paths.claude_agents_dir(),
    )?;
    for line in deploy_report_lines(&deploy, &paths.agent_source_dir()) {
        println!("  {line}");
    }
    println!("Framework installed. Run `trusty-mpm daemon` to start.");
    Ok(())
}

/// Render per-file status lines for an agent [`DeployResult`].
///
/// Why: `install` and `session start` both print agent deploy results; one
/// formatter keeps the output identical and the call sites small.
/// What: a `✓ <file> (composed: a → b → c)` line per deployed agent, a
/// `~ <file> (skipped — user-modified)` line per skipped one, and a `=` line
/// per unchanged one; the chain comes from the agent's resolved source chain.
/// Test: covered indirectly by `install_writes_all_artifacts`.
fn deploy_report_lines(
    deploy: &trusty_mpm_core::agent_deployer::DeployResult,
    source_dir: &std::path::Path,
) -> Vec<String> {
    let mut lines = Vec::new();
    for file in &deploy.deployed {
        let name = file.trim_end_matches(".md");
        let chain = trusty_mpm_core::agent_builder::source_chain(name, source_dir)
            .map(|c| c.join(" \u{2192} "))
            .unwrap_or_else(|_| name.to_string());
        lines.push(format!("\u{2713} {file} (composed: {chain})"));
    }
    for file in &deploy.skipped {
        lines.push(format!("~ {file} (skipped \u{2014} user-modified)"));
    }
    for file in &deploy.unchanged {
        lines.push(format!("= {file} (unchanged)"));
    }
    lines
}

/// Write every bundled artifact under `paths`, returning a per-file report.
///
/// Why: separating the filesystem work from argument parsing and stdout makes
/// the installer unit-testable against a `tempfile::TempDir`.
/// What: for each [`trusty_mpm_core::bundle::ALL`] artifact, creates parent
/// directories and writes the file; an existing file is skipped unless `force`.
/// Returns one human-readable status line per artifact.
/// Test: `install_writes_all_artifacts`, `install_skips_existing_without_force`.
fn install_to(
    paths: &trusty_mpm_core::paths::FrameworkPaths,
    force: bool,
) -> anyhow::Result<Vec<String>> {
    let mut report = Vec::new();
    for artifact in trusty_mpm_core::bundle::ALL {
        let dest = paths.framework.join(artifact.rel_path);
        if dest.exists() && !force {
            report.push(format!("- {} (exists, skipped)", artifact.rel_path));
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, artifact.contents)?;
        report.push(format!("\u{2713} {}", artifact.rel_path));
    }
    Ok(report)
}

/// Resolve a `--dir` option to an absolute path, defaulting to the cwd.
///
/// Why: `project` and `session` subcommands all accept an optional directory;
/// centralizing the "default to cwd" rule keeps the handlers uniform.
/// What: returns `dir` as a `PathBuf` when given, otherwise the process cwd.
/// Test: covered indirectly by the project/session handler integration tests.
fn resolve_dir(dir: Option<String>) -> anyhow::Result<std::path::PathBuf> {
    match dir {
        Some(d) => Ok(std::path::PathBuf::from(d)),
        None => Ok(std::env::current_dir()?),
    }
}

/// `project` subcommand — define and manage trusty-mpm projects.
///
/// Why: a project is a registered working directory; operators need shell
/// commands to register one, list all, and inspect the current one without
/// hand-crafting HTTP requests.
/// What: `Init` registers the directory (`POST /projects`) and scaffolds a
/// local `.trusty-mpm/`; `List` prints `GET /projects`; `Info` prints the
/// current directory's project via `GET /projects/current`.
/// Test: `cli_parses_project_init`, `cli_parses_project_list`,
/// `cli_parses_project_info`, `project_init_scaffolds_dotdir`.
async fn project(client: &reqwest::Client, url: &str, action: ProjectAction) -> anyhow::Result<()> {
    match action {
        ProjectAction::Init { dir } => {
            let path = resolve_dir(dir)?;
            let body: serde_json::Value = client
                .post(format!("{url}/projects"))
                .json(&serde_json::json!({ "path": path }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let report = scaffold_project_dir(&path)?;
            for line in &report {
                println!("  {line}");
            }
            let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            println!("registered project '{name}' at {}", path.display());
        }
        ProjectAction::List => {
            #[derive(Deserialize)]
            struct Body {
                projects: Vec<ProjectRow>,
            }
            let body: Body = client
                .get(format!("{url}/projects"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if body.projects.is_empty() {
                println!("no projects registered");
            }
            for p in &body.projects {
                println!("{} {}", p.name, p.path.display());
            }
        }
        ProjectAction::Info { dir } => {
            let path = resolve_dir(dir)?;
            let resp = client
                .get(format!("{url}/projects/current"))
                .query(&[("path", path.to_string_lossy().as_ref())])
                .send()
                .await?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                println!("{} is not a registered project", path.display());
            } else {
                let body: serde_json::Value = resp.error_for_status()?.json().await?;
                println!("{}", serde_json::to_string_pretty(&body)?);
            }
        }
    }
    Ok(())
}

/// Scaffold `<project>/.trusty-mpm/` with a config skeleton and `sessions/`.
///
/// Why: `project init` must give the operator an editable, version-controllable
/// project config; doing it in a testable helper keeps it covered without a
/// live daemon.
/// What: creates `.trusty-mpm/sessions/` and writes `config.toml` (only when
/// absent — never clobbering an edited config); returns a per-path report.
/// Test: `project_init_scaffolds_dotdir`, `project_init_keeps_existing_config`.
fn scaffold_project_dir(project: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let mut report = Vec::new();
    let dotdir = project.join(".trusty-mpm");
    let sessions = dotdir.join("sessions");
    std::fs::create_dir_all(&sessions)?;
    report.push(format!("\u{2713} {}", sessions.display()));

    let config = dotdir.join("config.toml");
    if config.exists() {
        report.push(format!("- {} (exists, skipped)", config.display()));
    } else {
        let name = trusty_mpm_core::project::name_from_path(project);
        let contents = format!(
            "# trusty-mpm project configuration\n\
             # Generated by: trusty-mpm project init\n\n\
             [project]\nname = \"{name}\"\n\n\
             [agents]\n\
             # Additional agent sources for this project\n\
             # sources = [\"https://example.com/agents\"]\n\n\
             [skills]\n\
             # Additional skill sources for this project\n\
             # sources = []\n"
        );
        std::fs::write(&config, contents)?;
        report.push(format!("\u{2713} {}", config.display()));
    }
    Ok(report)
}

/// `session` subcommand — define and manage sessions within a project.
///
/// Why: a session is a Claude Code instance; operators start, stop, list,
/// reap, and inspect them per project from the shell.
/// What: `Start` posts `POST /sessions` with the project path; `Stop` and
/// `Info` resolve a session by id or friendly name; `List` and `Clean` scope
/// to the project directory.
/// Test: `cli_parses_session_start`, `cli_parses_session_stop`,
/// `cli_parses_session_list`, `cli_parses_session_clean`,
/// `cli_parses_session_info`.
async fn session(client: &reqwest::Client, url: &str, action: SessionAction) -> anyhow::Result<()> {
    match action {
        SessionAction::Start { dir } => {
            let path = resolve_dir(dir)?;
            // Ensure `~/.claude/agents/` holds up-to-date composed agents
            // before the session launches; CC reads them at startup.
            let fw = trusty_mpm_core::paths::FrameworkPaths::default();
            match trusty_mpm_core::agent_deployer::deploy_agents(
                &fw.agent_source_dir(),
                &fw.claude_agents_dir(),
            ) {
                Ok(deploy) => println!(
                    "Agents: {} deployed, {} skipped, {} unchanged",
                    deploy.deployed.len(),
                    deploy.skipped.len(),
                    deploy.unchanged.len(),
                ),
                Err(err) => eprintln!("warning: agent deploy failed: {err}"),
            }
            #[derive(Deserialize)]
            struct Body {
                #[serde(default)]
                name: String,
            }
            let body: Body = client
                .post(format!("{url}/sessions"))
                .json(&serde_json::json!({
                    "workdir": path,
                    "project_path": path,
                }))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            // The daemon returns the friendly `tmpm-<adj>-<noun>` session name.
            println!("started session {}", body.name);
        }
        SessionAction::Stop { id_or_name } => {
            let resp = client
                .delete(format!("{url}/sessions/{id_or_name}"))
                .send()
                .await?;
            if resp.status() == reqwest::StatusCode::NOT_FOUND {
                println!("not found");
            } else {
                resp.error_for_status()?;
                println!("stopped {id_or_name}");
            }
        }
        SessionAction::List { dir } => {
            let path = resolve_dir(dir)?;
            #[derive(Deserialize)]
            struct Body {
                sessions: Vec<SessionRow>,
            }
            let body: Body = client
                .get(format!("{url}/sessions"))
                .query(&[("project", path.to_string_lossy().as_ref())])
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if body.sessions.is_empty() {
                println!("no sessions for {}", path.display());
            }
            for s in &body.sessions {
                let status = s.status.as_str().unwrap_or("unknown");
                println!("{} {} {}", short_id(&s.id), status, s.workdir);
            }
        }
        SessionAction::Clean { dir } => {
            // `dir` is accepted for symmetry; the daemon reaps globally.
            let _ = resolve_dir(dir)?;
            let body: serde_json::Value = client
                .delete(format!("{url}/sessions/dead"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let removed = body.get("removed").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("reaped {removed} dead session(s)");
        }
        SessionAction::Info { id_or_name } => {
            #[derive(Deserialize)]
            struct Body {
                sessions: Vec<serde_json::Value>,
            }
            let body: Body = client
                .get(format!("{url}/sessions"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            let found = body.sessions.iter().find(|s| {
                let id_match = s
                    .get("id")
                    .and_then(|v| v.get("0"))
                    .and_then(|v| v.as_str())
                    == Some(id_or_name.as_str());
                let name_match =
                    s.get("tmux_name").and_then(|v| v.as_str()) == Some(id_or_name.as_str());
                id_match || name_match
            });
            match found {
                Some(s) => println!("{}", serde_json::to_string_pretty(s)?),
                None => println!("session '{id_or_name}' not found"),
            }
        }
    }
    Ok(())
}

/// Inspect or configure the token-use optimizer.
///
/// Why: the optimizer policy is framework-managed on disk; `Status` reads the
/// daemon's live view via `GET /optimizer`, while `Set` rewrites the policy
/// file itself (`~/.trusty-mpm/framework/hooks/optimizer.toml`) — the daemon's
/// watcher then reloads it.
/// What: `Status` prints the current config; `Set` writes a new `[default]`
/// level into the policy file, creating the `hooks/` directory if needed.
/// Test: `cli_parses_optimizer_status`, `cli_parses_optimizer_set`.
async fn optimizer(
    client: &reqwest::Client,
    url: &str,
    action: OptimizerAction,
) -> anyhow::Result<()> {
    match action {
        OptimizerAction::Status => {
            let body: serde_json::Value = client
                .get(format!("{url}/optimizer"))
                .send()
                .await?
                .json()
                .await?;
            println!("{}", serde_json::to_string_pretty(&body["optimizer"])?);
        }
        OptimizerAction::Set { level } => {
            let level_name = match level {
                CliCompressionLevel::Off => "Off",
                CliCompressionLevel::Trim => "Trim",
                CliCompressionLevel::Summarise => "Summarise",
                CliCompressionLevel::Caveman => "Caveman",
            };
            let paths = trusty_mpm_core::paths::FrameworkPaths::default();
            let path = paths.optimizer_config();
            std::fs::create_dir_all(&paths.hooks)?;
            let contents = format!(
                "# trusty-mpm token optimizer — framework hook configuration\n\
                 # Edited by: trusty-mpm optimizer set\n\n\
                 [default]\nlevel = \"{level_name}\"\n\n\
                 [tools]\n"
            );
            std::fs::write(&path, contents)?;
            println!("optimizer level set to {level_name} ({})", path.display());
        }
    }
    Ok(())
}

/// Render a `SessionId` newtype JSON value into a short, human id.
///
/// Why: the daemon serializes `SessionId` as `{"0": "<uuid>"}`; the CLI shows
/// only the first 8 characters so rows stay compact.
/// What: extracts the inner UUID string and truncates it, falling back to a
/// placeholder if the shape is unexpected.
/// Test: covered by the `short_id_*` unit tests below.
fn short_id(value: &serde_json::Value) -> String {
    value
        .get("0")
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "????????".to_string())
}

/// `status` subcommand — probe daemon health and list sessions.
///
/// Why: the first thing an operator runs to see if the daemon is alive.
/// What: `GET /health` then `GET /sessions`, printing one line per session.
/// Test: run against a live daemon; "daemon: unreachable" when it is down.
async fn status(client: &reqwest::Client, url: &str) -> anyhow::Result<()> {
    let healthy = match client.get(format!("{url}/health")).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    };
    if !healthy {
        println!("daemon: unreachable");
        return Ok(());
    }
    println!("daemon: ok");

    #[derive(Deserialize)]
    struct Body {
        sessions: Vec<SessionRow>,
    }
    let body: Body = client
        .get(format!("{url}/sessions"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for s in &body.sessions {
        let status = s.status.as_str().unwrap_or("unknown");
        println!(
            "{} {} {} ({} delegations)",
            short_id(&s.id),
            status,
            s.workdir,
            s.active_delegations
        );
    }
    Ok(())
}

/// `events` subcommand — print the recent hook-event feed.
///
/// Why: gives operators a quick tail of daemon activity without the TUI.
/// What: `GET /events`, printing `{timestamp} {session_short} {event}`.
/// Test: run against a daemon that has ingested hook events.
async fn events(client: &reqwest::Client, url: &str) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Body {
        events: Vec<EventRow>,
    }
    let body: Body = client
        .get(format!("{url}/events"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    for e in &body.events {
        println!("{} {} {}", e.at, short_id(&e.session), e.event);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn short_id_extracts_uuid_prefix() {
        // SessionId newtype shape `{"0": "<uuid>"}` → first 8 chars of the uuid.
        let value = serde_json::json!({"0": "abcd1234-5678-90ab-cdef-1234567890ab"});
        assert_eq!(short_id(&value), "abcd1234");
    }

    #[test]
    fn short_id_truncates_to_eight_chars() {
        // Any inner uuid string must collapse to exactly 8 characters.
        let value = serde_json::json!({"0": "0123456789abcdef-rest-ignored"});
        assert_eq!(short_id(&value).chars().count(), 8);
    }

    #[test]
    fn short_id_falls_back_when_field_missing() {
        // Missing `0` key or a scalar value → the placeholder.
        assert_eq!(short_id(&serde_json::json!({})), "????????");
        assert_eq!(short_id(&serde_json::json!("scalar")), "????????");
    }

    #[test]
    fn short_id_falls_back_when_value_not_str() {
        // `0` present but not a string → the placeholder.
        assert_eq!(short_id(&serde_json::json!({"0": 42})), "????????");
    }

    #[test]
    fn cli_parses_status() {
        let cli = Cli::try_parse_from(["trusty-mpm", "status"]).unwrap();
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn cli_parses_project_init() {
        let cli = Cli::try_parse_from(["trusty-mpm", "project", "init"]).unwrap();
        match cli.command {
            Command::Project {
                action: ProjectAction::Init { dir },
            } => assert_eq!(dir, None),
            other => panic!("expected project init, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_project_init_with_dir() {
        let cli =
            Cli::try_parse_from(["trusty-mpm", "project", "init", "--dir", "/work/p"]).unwrap();
        match cli.command {
            Command::Project {
                action: ProjectAction::Init { dir },
            } => assert_eq!(dir.as_deref(), Some("/work/p")),
            other => panic!("expected project init, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_project_list() {
        let cli = Cli::try_parse_from(["trusty-mpm", "project", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Project {
                action: ProjectAction::List
            }
        ));
    }

    #[test]
    fn cli_parses_project_info() {
        let cli = Cli::try_parse_from(["trusty-mpm", "project", "info"]).unwrap();
        match cli.command {
            Command::Project {
                action: ProjectAction::Info { dir },
            } => assert_eq!(dir, None),
            other => panic!("expected project info, got {other:?}"),
        }
    }

    #[test]
    fn cli_project_requires_action() {
        // `project` with no action is an error.
        assert!(Cli::try_parse_from(["trusty-mpm", "project"]).is_err());
    }

    #[test]
    fn cli_parses_session_start() {
        let cli = Cli::try_parse_from(["trusty-mpm", "session", "start"]).unwrap();
        match cli.command {
            Command::Session {
                action: SessionAction::Start { dir },
            } => assert_eq!(dir, None),
            other => panic!("expected session start, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_session_start_with_dir() {
        let cli =
            Cli::try_parse_from(["trusty-mpm", "session", "start", "--dir", "/work/p"]).unwrap();
        match cli.command {
            Command::Session {
                action: SessionAction::Start { dir },
            } => assert_eq!(dir.as_deref(), Some("/work/p")),
            other => panic!("expected session start, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_session_stop() {
        let cli =
            Cli::try_parse_from(["trusty-mpm", "session", "stop", "tmpm-quiet-falcon"]).unwrap();
        match cli.command {
            Command::Session {
                action: SessionAction::Stop { id_or_name },
            } => assert_eq!(id_or_name, "tmpm-quiet-falcon"),
            other => panic!("expected session stop, got {other:?}"),
        }
    }

    #[test]
    fn cli_session_stop_requires_arg() {
        // `session stop` without an id-or-name is an error.
        assert!(Cli::try_parse_from(["trusty-mpm", "session", "stop"]).is_err());
    }

    #[test]
    fn cli_parses_session_list() {
        let cli = Cli::try_parse_from(["trusty-mpm", "session", "list"]).unwrap();
        match cli.command {
            Command::Session {
                action: SessionAction::List { dir },
            } => assert_eq!(dir, None),
            other => panic!("expected session list, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_session_clean() {
        let cli = Cli::try_parse_from(["trusty-mpm", "session", "clean"]).unwrap();
        match cli.command {
            Command::Session {
                action: SessionAction::Clean { dir },
            } => assert_eq!(dir, None),
            other => panic!("expected session clean, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_session_info() {
        let cli = Cli::try_parse_from(["trusty-mpm", "session", "info", "abc-123"]).unwrap();
        match cli.command {
            Command::Session {
                action: SessionAction::Info { id_or_name },
            } => assert_eq!(id_or_name, "abc-123"),
            other => panic!("expected session info, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_events() {
        let cli = Cli::try_parse_from(["trusty-mpm", "events"]).unwrap();
        assert!(matches!(cli.command, Command::Events));
    }

    #[test]
    fn cli_url_flag_overrides_default() {
        let cli = Cli::try_parse_from(["trusty-mpm", "--url", "http://x:9", "status"]).unwrap();
        assert_eq!(cli.url, "http://x:9");
    }

    #[test]
    fn cli_rejects_no_subcommand() {
        // A subcommand is mandatory; bare invocation must error.
        assert!(Cli::try_parse_from(["trusty-mpm"]).is_err());
    }

    #[test]
    fn cli_parses_tui_defaults() {
        let cli = Cli::try_parse_from(["trusty-mpm", "tui"]).unwrap();
        match cli.command {
            Command::Tui { url, interval_ms } => {
                assert_eq!(url, DEFAULT_URL);
                assert_eq!(interval_ms, 1000);
            }
            other => panic!("expected Tui, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_tui_with_interval() {
        let cli = Cli::try_parse_from(["trusty-mpm", "tui", "--interval-ms", "500"]).unwrap();
        match cli.command {
            Command::Tui { interval_ms, .. } => assert_eq!(interval_ms, 500),
            other => panic!("expected Tui, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_telegram_with_check() {
        let cli = Cli::try_parse_from(["trusty-mpm", "telegram", "--check"]).unwrap();
        match cli.command {
            Command::Telegram { check, token, .. } => {
                assert!(check);
                assert_eq!(token, None);
            }
            other => panic!("expected Telegram, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_telegram_with_token() {
        let cli = Cli::try_parse_from(["trusty-mpm", "telegram", "--token", "secret"]).unwrap();
        match cli.command {
            Command::Telegram { token, check, .. } => {
                assert_eq!(token.as_deref(), Some("secret"));
                assert!(!check);
            }
            other => panic!("expected Telegram, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_daemon_defaults() {
        let cli = Cli::try_parse_from(["trusty-mpm", "daemon"]).unwrap();
        match cli.command {
            Command::Daemon { addr, mcp } => {
                assert_eq!(addr.to_string(), "127.0.0.1:7880");
                assert!(!mcp);
            }
            other => panic!("expected Daemon, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_daemon_mcp() {
        let cli = Cli::try_parse_from(["trusty-mpm", "daemon", "--mcp"]).unwrap();
        match cli.command {
            Command::Daemon { mcp, .. } => assert!(mcp),
            other => panic!("expected Daemon, got {other:?}"),
        }
    }

    #[test]
    fn project_init_scaffolds_dotdir() {
        // `project init` must create `.trusty-mpm/{config.toml,sessions/}`
        // with a config skeleton naming the project after its directory.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("my-app");
        std::fs::create_dir_all(&project).unwrap();
        let report = scaffold_project_dir(&project).unwrap();
        assert_eq!(report.len(), 2);

        let config = project.join(".trusty-mpm/config.toml");
        let sessions = project.join(".trusty-mpm/sessions");
        assert!(config.exists());
        assert!(sessions.is_dir());
        let contents = std::fs::read_to_string(&config).unwrap();
        assert!(contents.contains("name = \"my-app\""));
        assert!(contents.contains("[agents]"));
        assert!(contents.contains("[skills]"));
    }

    #[test]
    fn project_init_keeps_existing_config() {
        // Re-running `project init` must never clobber an edited config.toml.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().to_path_buf();
        scaffold_project_dir(&project).unwrap();
        let config = project.join(".trusty-mpm/config.toml");
        std::fs::write(&config, "# edited by hand").unwrap();

        let report = scaffold_project_dir(&project).unwrap();
        assert!(report.iter().any(|l| l.contains("skipped")));
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "# edited by hand"
        );
    }

    #[test]
    fn cli_parses_install_no_force() {
        let cli = Cli::try_parse_from(["trusty-mpm", "install"]).unwrap();
        match cli.command {
            Command::Install { force } => assert!(!force),
            other => panic!("expected Install, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_install_with_force() {
        let cli = Cli::try_parse_from(["trusty-mpm", "install", "--force"]).unwrap();
        match cli.command {
            Command::Install { force } => assert!(force),
            other => panic!("expected Install, got {other:?}"),
        }
    }

    #[test]
    fn install_writes_all_artifacts() {
        // A fresh install must write every bundled artifact to disk under the
        // framework root, with matching content.
        let dir = tempfile::tempdir().unwrap();
        let paths = trusty_mpm_core::paths::FrameworkPaths::under(dir.path());
        let report = install_to(&paths, false).unwrap();
        assert_eq!(report.len(), trusty_mpm_core::bundle::ALL.len());
        for artifact in trusty_mpm_core::bundle::ALL {
            let dest = paths.framework.join(artifact.rel_path);
            assert!(dest.exists(), "missing {}", artifact.rel_path);
            let written = std::fs::read_to_string(&dest).unwrap();
            assert_eq!(written, artifact.contents);
        }
    }

    #[test]
    fn install_skips_existing_without_force() {
        // An existing artifact is left untouched without `--force` and the
        // report says so; `--force` overwrites it.
        let dir = tempfile::tempdir().unwrap();
        let paths = trusty_mpm_core::paths::FrameworkPaths::under(dir.path());
        let optimizer = paths.optimizer_config();
        std::fs::create_dir_all(&paths.hooks).unwrap();
        std::fs::write(&optimizer, "custom").unwrap();

        let report = install_to(&paths, false).unwrap();
        assert!(report.iter().any(|l| l.contains("skipped")));
        assert_eq!(std::fs::read_to_string(&optimizer).unwrap(), "custom");

        let forced = install_to(&paths, true).unwrap();
        assert!(forced.iter().all(|l| !l.contains("skipped")));
        assert_ne!(std::fs::read_to_string(&optimizer).unwrap(), "custom");
    }

    #[test]
    fn install_then_deploy_composes_agents() {
        // Installing the bundled agent sources and then deploying them must
        // produce composed, inheritance-flattened files in `.claude/agents/`.
        let dir = tempfile::tempdir().unwrap();
        let paths = trusty_mpm_core::paths::FrameworkPaths::under(dir.path());
        install_to(&paths, false).unwrap();

        let result = trusty_mpm_core::agent_deployer::deploy_agents(
            &paths.agent_source_dir(),
            &paths.claude_agents_dir(),
        )
        .unwrap();
        // All six bundled agents deploy on a fresh target.
        assert_eq!(result.deployed.len(), 6);
        assert!(result.skipped.is_empty());

        // The composed engineer carries inherited base content and no
        // `extends:` for Claude Code to interpret.
        let engineer =
            std::fs::read_to_string(paths.claude_agents_dir().join("engineer.md")).unwrap();
        assert!(engineer.contains("BASE-AGENT"));
        assert!(engineer.contains("BASE-ENGINEER"));
        assert!(engineer.contains("# Engineer"));
        assert!(!engineer.contains("extends:"));

        // The report formatter renders a composed-chain line.
        let lines = deploy_report_lines(&result, &paths.agent_source_dir());
        assert!(
            lines
                .iter()
                .any(|l| l.contains("engineer.md") && l.contains("composed:")),
            "lines = {lines:?}"
        );
    }

    #[test]
    fn cli_parses_daemon_custom_addr() {
        let cli = Cli::try_parse_from(["trusty-mpm", "daemon", "--addr", "0.0.0.0:9000"]).unwrap();
        match cli.command {
            Command::Daemon { addr, .. } => assert_eq!(addr.to_string(), "0.0.0.0:9000"),
            other => panic!("expected Daemon, got {other:?}"),
        }
    }
}
