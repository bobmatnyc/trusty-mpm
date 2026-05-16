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

fn main() -> anyhow::Result<()> {
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
    // The teloxide dispatcher (long-polling, command handlers, alert pusher)
    // is wired in the Telegram milestone issue; the decision logic it depends
    // on — `commands::parse` and `alerts::*` — is already implemented and
    // tested here.
    anyhow::bail!(
        "Telegram runtime not yet wired (token len {}); see issue tracker",
        token.len()
    )
}
