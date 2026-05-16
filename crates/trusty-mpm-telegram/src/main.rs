//! trusty-mpm Telegram bot.
//!
//! Why: remote management lets an operator drive the daemon from a phone —
//! list sessions, check status, approve a pending permission request, and
//! receive memory-pressure / hook-event alerts — without a terminal.
//! What: parses operator commands via [`commands`] and decides/format alerts
//! via [`alerts`]. The teloxide runtime wiring is intentionally thin; all
//! decision logic lives in the two unit-tested modules.
//! Test: `cargo test -p trusty-mpm-telegram` covers command parsing and alert
//! formatting; `cargo run -p trusty-mpm-telegram -- --check` validates config.

mod alerts;
mod commands;

use clap::Parser;
use teloxide::prelude::*;

use alerts::AlertConfig;

/// trusty-mpm Telegram bot command-line options.
#[derive(Debug, Parser)]
#[command(
    name = "trusty-mpm-telegram",
    version,
    about = "trusty-mpm Telegram bot"
)]
struct Args {
    /// Base URL of the trusty-mpm daemon.
    #[arg(long, env = "TRUSTY_MPM_URL", default_value = "http://127.0.0.1:7880")]
    url: String,

    /// Telegram bot token (read from the environment in production).
    #[arg(long, env = "TELEGRAM_BOT_TOKEN")]
    token: Option<String>,

    /// Validate configuration and exit without connecting to Telegram.
    #[arg(long)]
    check: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let alert_config = AlertConfig::recommended();

    if args.check {
        println!("trusty-mpm Telegram bot configuration:");
        println!("  daemon url        : {}", args.url);
        println!(
            "  token configured  : {}",
            if args.token.is_some() { "yes" } else { "no" }
        );
        println!("  alert categories  : {:?}", alert_config.categories);
        println!("  memory alerts     : {}", alert_config.memory_alerts);
        println!();
        println!("{}", commands::help_text());
        return Ok(());
    }

    let token = args.token.ok_or_else(|| {
        anyhow::anyhow!("TELEGRAM_BOT_TOKEN is required (or pass --check to validate config)")
    })?;

    // Run the teloxide long-polling repl: every text message is parsed into a
    // `BotCommand` and dispatched against the daemon's HTTP API.
    let bot = Bot::new(token);
    let daemon_url = args.url.clone();

    teloxide::repl(bot, move |bot: Bot, msg: Message| {
        let daemon_url = daemon_url.clone();
        async move {
            if let Some(text) = msg.text() {
                let reply = match commands::parse(text) {
                    Ok(cmd) => handle_command(cmd, &daemon_url).await,
                    Err(e) => e,
                };
                bot.send_message(msg.chat.id, reply).await?;
            }
            Ok(())
        }
    })
    .await;
    Ok(())
}

/// Dispatch a parsed operator command against the daemon HTTP API.
///
/// Why: keeps the teloxide closure thin — all daemon I/O and reply formatting
/// lives here where it can evolve independently of the runtime wiring.
/// What: maps each [`commands::BotCommand`] to a daemon request and renders a
/// human-readable reply string; unreachable-daemon and parse errors become the
/// reply text rather than panics.
/// Test: covered indirectly by `commands` parsing tests; live behaviour is
/// exercised by running the bot against a daemon.
async fn handle_command(cmd: commands::BotCommand, daemon_url: &str) -> String {
    use commands::BotCommand::*;
    let client = reqwest::Client::new();
    match cmd {
        Help => commands::help_text().to_string(),
        Sessions => {
            let url = format!("{daemon_url}/sessions");
            match client.get(&url).send().await {
                Ok(r) => match r.json::<serde_json::Value>().await {
                    Ok(body) => {
                        let sessions = body["sessions"].as_array().cloned().unwrap_or_default();
                        if sessions.is_empty() {
                            "No active sessions.".into()
                        } else {
                            sessions
                                .iter()
                                .map(|s| {
                                    format!(
                                        "{} {} {}",
                                        s["id"].as_str().unwrap_or("?"),
                                        s["status"].as_str().unwrap_or("?"),
                                        s["workdir"].as_str().unwrap_or("?")
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                    }
                    Err(e) => format!("parse error: {e}"),
                },
                Err(e) => format!("daemon unreachable: {e}"),
            }
        }
        Status { session_id } => {
            let url = format!("{daemon_url}/sessions/{session_id}/events");
            match client.get(&url).send().await {
                Ok(r) => match r.json::<serde_json::Value>().await {
                    Ok(body) => {
                        let events = body["events"].as_array().cloned().unwrap_or_default();
                        let last5: Vec<_> = events.iter().rev().take(5).collect();
                        if last5.is_empty() {
                            format!("Session {session_id}: no recent events")
                        } else {
                            last5
                                .iter()
                                .map(|e| e["event"].as_str().unwrap_or("?").to_string())
                                .collect::<Vec<_>>()
                                .join("\n")
                        }
                    }
                    Err(e) => format!("parse error: {e}"),
                },
                Err(e) => format!("daemon unreachable: {e}"),
            }
        }
        Approve { session_id } | Deny { session_id } => {
            format!("Permission approval for {session_id} not yet wired to daemon API")
        }
    }
}
