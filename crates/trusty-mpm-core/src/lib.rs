//! # trusty-mpm-core
//!
//! Why: Shared types used by every trusty-mpm crate (daemon, CLI, TUI, Telegram).
//! Centralizing them prevents protocol drift between the daemon and its clients.
//!
//! What: Defines the artifact model (agents, skills, hooks), session state types,
//! and the IPC protocol envelope exchanged over the daemon's local socket / HTTP API.
//!
//! Test: `cargo test -p trusty-mpm-core` exercises serde round-trips and the
//! claude-mpm frontmatter parser against fixture files.

pub mod agent;
pub mod agent_builder;
pub mod agent_deployer;
pub mod agent_manifest;
pub mod artifact;
pub mod budget;
pub mod bundle;
pub mod circuit;
pub mod compress;
pub mod deterministic_overseer;
pub mod error;
pub mod hook;
pub mod ipc;
pub mod llm_overseer;
pub mod memory;
pub mod names;
pub mod overseer;
pub mod overseer_config;
pub mod paths;
pub mod project;
pub mod session;
pub mod tmux;

pub use error::{Error, Result};
