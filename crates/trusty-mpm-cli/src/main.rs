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
    /// Start a new session.
    Start {
        /// Working directory for the new session (defaults to the cwd).
        #[arg(long)]
        workdir: Option<String>,
    },
    /// Stop a running session.
    Stop {
        /// Session id to stop.
        id: String,
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
    /// Manage the daemon's session registry.
    Sessions {
        /// Session-registry action to perform.
        #[command(subcommand)]
        action: SessionsAction,
    },
}

/// Actions for the `sessions` subcommand.
#[derive(Debug, Subcommand)]
enum SessionsAction {
    /// Reap registry entries whose tmux session has exited.
    Clean,
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
        Command::Start { workdir } => start(&client, &cli.url, workdir).await,
        Command::Stop { id } => stop(&client, &cli.url, &id).await,
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
        Command::Sessions { action } => sessions(&client, &cli.url, action).await,
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
    println!("Framework installed. Run `trusty-mpm daemon` to start.");
    Ok(())
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

/// `sessions` subcommand — manage the daemon's session registry.
///
/// Why: dead tmux sessions leave stale registry entries; operators need a
/// shell command to prune them without hand-crafting an HTTP request.
/// What: `Clean` issues `DELETE /sessions/dead` and prints how many entries
/// the daemon reaped.
/// Test: `cli_parses_sessions_clean`.
async fn sessions(
    client: &reqwest::Client,
    url: &str,
    action: SessionsAction,
) -> anyhow::Result<()> {
    match action {
        SessionsAction::Clean => {
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

/// `start` subcommand — register a new session with the daemon.
///
/// Why: launches a managed session without the operator touching the API.
/// What: `POST /sessions { "workdir": ... }`, defaulting to the current dir.
/// Test: run `trusty-mpm start`; prints `started session {id}`.
async fn start(client: &reqwest::Client, url: &str, workdir: Option<String>) -> anyhow::Result<()> {
    let workdir = match workdir {
        Some(w) => w,
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    #[derive(Deserialize)]
    struct Body {
        id: serde_json::Value,
    }
    let body: Body = client
        .post(format!("{url}/sessions"))
        .json(&serde_json::json!({ "workdir": workdir }))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let id = body
        .id
        .get("0")
        .and_then(|v| v.as_str())
        .unwrap_or("<unknown>");
    println!("started session {id}");
    Ok(())
}

/// `stop` subcommand — deregister a session by id.
///
/// Why: lets an operator tear a session down from the shell.
/// What: `DELETE /sessions/{id}`; a `404` prints `not found`.
/// Test: stop a known id then an unknown one to see both branches.
async fn stop(client: &reqwest::Client, url: &str, id: &str) -> anyhow::Result<()> {
    let resp = client.delete(format!("{url}/sessions/{id}")).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        println!("not found");
    } else {
        resp.error_for_status()?;
        println!("stopped {id}");
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
    fn cli_parses_start_no_args() {
        let cli = Cli::try_parse_from(["trusty-mpm", "start"]).unwrap();
        match cli.command {
            Command::Start { workdir } => assert_eq!(workdir, None),
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_start_with_workdir() {
        let cli = Cli::try_parse_from(["trusty-mpm", "start", "--workdir", "/tmp"]).unwrap();
        match cli.command {
            Command::Start { workdir } => assert_eq!(workdir.as_deref(), Some("/tmp")),
            other => panic!("expected Start, got {other:?}"),
        }
    }

    #[test]
    fn cli_parses_stop() {
        let cli = Cli::try_parse_from(["trusty-mpm", "stop", "abc-123"]).unwrap();
        match cli.command {
            Command::Stop { id } => assert_eq!(id, "abc-123"),
            other => panic!("expected Stop, got {other:?}"),
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
    fn cli_parses_sessions_clean() {
        let cli = Cli::try_parse_from(["trusty-mpm", "sessions", "clean"]).unwrap();
        match cli.command {
            Command::Sessions { action } => {
                assert!(matches!(action, SessionsAction::Clean));
            }
            other => panic!("expected Sessions, got {other:?}"),
        }
    }

    #[test]
    fn cli_sessions_requires_action() {
        // `sessions` with no action is an error — `clean` is mandatory.
        assert!(Cli::try_parse_from(["trusty-mpm", "sessions"]).is_err());
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
    fn cli_parses_daemon_custom_addr() {
        let cli = Cli::try_parse_from(["trusty-mpm", "daemon", "--addr", "0.0.0.0:9000"]).unwrap();
        match cli.command {
            Command::Daemon { addr, .. } => assert_eq!(addr.to_string(), "0.0.0.0:9000"),
            other => panic!("expected Daemon, got {other:?}"),
        }
    }
}
