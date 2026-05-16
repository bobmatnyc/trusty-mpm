//! trusty-mpm CLI client.
//!
//! Why: Users and scripts need a thin, fast client that talks to the daemon
//! over IPC instead of orchestrating Claude Code directly.
//! What: Parses subcommands and (in this scaffold) prints the resolved command.
//! Test: `cargo run -p trusty-mpm-cli -- status` should print the status command.

use clap::{Parser, Subcommand};

/// trusty-mpm command-line interface.
#[derive(Debug, Parser)]
#[command(name = "trusty-mpm", version, about = "trusty-mpm CLI")]
struct Cli {
    /// Subcommand to run.
    #[command(subcommand)]
    command: Command,
}

/// Top-level CLI subcommands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Show daemon and session status.
    Status,
    /// Start a new session in the current directory.
    Start,
    /// Stop a running session.
    Stop,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Status => println!("trusty-mpm: status (scaffold)"),
        Command::Start => println!("trusty-mpm: start (scaffold)"),
        Command::Stop => println!("trusty-mpm: stop (scaffold)"),
    }
    Ok(())
}
