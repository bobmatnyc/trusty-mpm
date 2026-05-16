//! trusty-mpm Telegram bot shim (`trusty-mpm-telegram`).
//!
//! Why: kept as a backward-compatible standalone binary — the primary entry
//! point is now `trusty-mpm telegram`, which calls the same
//! [`trusty_mpm_telegram::run`].
//! What: parses CLI flags and delegates to the library's `run`.
//! Test: `cargo run -p trusty-mpm-telegram -- --check` validates config.

use clap::Parser;

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
    trusty_mpm_telegram::run(args.url, args.token, args.check).await
}
