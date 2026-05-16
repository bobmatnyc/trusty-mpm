//! trusty-mpm Telegram bot.
//!
//! Why: Remote management (start/stop sessions, approve permission requests)
//! lets an operator drive the daemon from a phone without a terminal.
//! What: teloxide-based bot. This scaffold prints a placeholder; command
//! handlers land in the Telegram milestone issues.
//! Test: `cargo run -p trusty-mpm-telegram` should print the placeholder line.

fn main() -> anyhow::Result<()> {
    println!("trusty-mpm Telegram bot (scaffold) — command handlers pending");
    Ok(())
}
