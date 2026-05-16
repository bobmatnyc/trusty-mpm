//! trusty-mpm TUI dashboard.
//!
//! Why: Operators need an at-a-glance view of active sessions, agent
//! delegations, and circuit-breaker state without parsing logs.
//! What: ratatui-based dashboard. This scaffold prints a placeholder; real
//! widgets land in the Dashboard milestone issues.
//! Test: `cargo run -p trusty-mpm-tui` should print the placeholder line.

fn main() -> anyhow::Result<()> {
    println!("trusty-mpm TUI dashboard (scaffold) — ratatui widgets pending");
    Ok(())
}
