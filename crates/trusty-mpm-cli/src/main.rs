//! trusty-mpm CLI client.
//!
//! Why: Users and scripts need a thin, fast client that talks to the daemon
//! over HTTP instead of orchestrating Claude Code directly.
//! What: parses subcommands and drives the daemon's HTTP API with a blocking
//! `reqwest` client — status, session start/stop, and the event feed.
//! Test: `cargo run -p trusty-mpm-cli -- status` prints daemon/session state;
//! handler logic is covered by `cargo test -p trusty-mpm-cli`.

use clap::{Parser, Subcommand};
use serde::Deserialize;

/// Default daemon address when `--url` / `TRUSTY_MPM_URL` is unset.
const DEFAULT_URL: &str = "http://127.0.0.1:7880";

/// trusty-mpm command-line interface.
#[derive(Debug, Parser)]
#[command(name = "trusty-mpm", version, about = "trusty-mpm CLI")]
struct Cli {
    /// Base URL of the trusty-mpm daemon.
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = reqwest::blocking::Client::new();
    match cli.command {
        Command::Status => status(&client, &cli.url),
        Command::Start { workdir } => start(&client, &cli.url, workdir),
        Command::Stop { id } => stop(&client, &cli.url, &id),
        Command::Events => events(&client, &cli.url),
    }
}

/// Render a `SessionId` newtype JSON value into a short, human id.
///
/// Why: the daemon serializes `SessionId` as `{"0": "<uuid>"}`; the CLI shows
/// only the first 8 characters so rows stay compact.
/// What: extracts the inner UUID string and truncates it, falling back to a
/// placeholder if the shape is unexpected.
/// Test: covered indirectly by `status`/`events` integration runs.
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
fn status(client: &reqwest::blocking::Client, url: &str) -> anyhow::Result<()> {
    let healthy = client
        .get(format!("{url}/health"))
        .send()
        .map(|r| r.status().is_success())
        .unwrap_or(false);
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
        .send()?
        .error_for_status()?
        .json()?;
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
fn start(
    client: &reqwest::blocking::Client,
    url: &str,
    workdir: Option<String>,
) -> anyhow::Result<()> {
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
        .send()?
        .error_for_status()?
        .json()?;
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
fn stop(client: &reqwest::blocking::Client, url: &str, id: &str) -> anyhow::Result<()> {
    let resp = client.delete(format!("{url}/sessions/{id}")).send()?;
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
fn events(client: &reqwest::blocking::Client, url: &str) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Body {
        events: Vec<EventRow>,
    }
    let body: Body = client
        .get(format!("{url}/events"))
        .send()?
        .error_for_status()?
        .json()?;
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
}
