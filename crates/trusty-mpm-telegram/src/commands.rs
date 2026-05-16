//! Telegram operator command parsing.
//!
//! Why: the bot lets an operator drive the daemon from a phone — list sessions,
//! check status, approve a pending permission request. Parsing the chat text
//! into a typed command is pure logic that should be tested without teloxide.
//! What: [`BotCommand`] enumerates the supported commands; [`parse`] turns a
//! raw message string into one (or a parse error).
//! Test: `cargo test -p trusty-mpm-telegram` covers every command and the
//! error paths.

// The teloxide command-dispatch loop that calls `parse` lands in a follow-up
// issue; until then the parser is exercised only by its own tests.
#![allow(dead_code)]

/// A parsed operator command from a Telegram message.
///
/// Why: a typed command keeps the bot's dispatch exhaustive and the parser
/// testable.
/// What: the small set of remote-management actions trusty-mpm supports.
/// Test: see the `parse_*` tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BotCommand {
    /// `/sessions` — list all managed sessions.
    Sessions,
    /// `/status <id>` — show detailed status for one session.
    Status { session_id: String },
    /// `/approve <id>` — approve a session's pending permission request.
    Approve { session_id: String },
    /// `/deny <id>` — deny a session's pending permission request.
    Deny { session_id: String },
    /// `/help` — show the command list.
    Help,
}

/// Parse a raw Telegram message into a [`BotCommand`].
///
/// Why: one entry point keeps command syntax in a single, tested place.
/// What: matches the leading `/word`; commands that need an argument
/// (`/status`, `/approve`, `/deny`) require exactly one. Returns `Err` with a
/// human-readable reason the bot can echo back to the operator.
/// Test: `parse_sessions`, `parse_status_requires_argument`, etc.
pub fn parse(message: &str) -> Result<BotCommand, String> {
    let mut parts = message.split_whitespace();
    let verb = parts.next().ok_or_else(|| "empty message".to_string())?;
    let arg = parts.next();
    // Reject trailing junk so `/status a b` is an explicit error.
    if parts.next().is_some() {
        return Err(format!("too many arguments for `{verb}`"));
    }

    match verb {
        "/sessions" => Ok(BotCommand::Sessions),
        "/help" => Ok(BotCommand::Help),
        "/status" => Ok(BotCommand::Status {
            session_id: require_arg(verb, arg)?,
        }),
        "/approve" => Ok(BotCommand::Approve {
            session_id: require_arg(verb, arg)?,
        }),
        "/deny" => Ok(BotCommand::Deny {
            session_id: require_arg(verb, arg)?,
        }),
        other => Err(format!("unknown command: `{other}` (try /help)")),
    }
}

/// Require that a command was given exactly one argument.
fn require_arg(verb: &str, arg: Option<&str>) -> Result<String, String> {
    arg.map(str::to_string)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("`{verb}` needs a session id"))
}

/// The `/help` text listing every command.
pub fn help_text() -> &'static str {
    "trusty-mpm bot commands:\n\
     /sessions — list managed sessions\n\
     /status <id> — detailed session status\n\
     /approve <id> — approve a pending permission request\n\
     /deny <id> — deny a pending permission request\n\
     /help — this message"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sessions() {
        assert_eq!(parse("/sessions").unwrap(), BotCommand::Sessions);
        // Surrounding whitespace is tolerated.
        assert_eq!(parse("  /sessions  ").unwrap(), BotCommand::Sessions);
    }

    #[test]
    fn parse_help() {
        assert_eq!(parse("/help").unwrap(), BotCommand::Help);
        assert!(help_text().contains("/approve"));
    }

    #[test]
    fn parse_status_with_argument() {
        assert_eq!(
            parse("/status abc-123").unwrap(),
            BotCommand::Status {
                session_id: "abc-123".into()
            }
        );
    }

    #[test]
    fn parse_status_requires_argument() {
        let err = parse("/status").unwrap_err();
        assert!(err.contains("needs a session id"));
    }

    #[test]
    fn parse_approve_and_deny() {
        assert_eq!(
            parse("/approve s1").unwrap(),
            BotCommand::Approve {
                session_id: "s1".into()
            }
        );
        assert_eq!(
            parse("/deny s2").unwrap(),
            BotCommand::Deny {
                session_id: "s2".into()
            }
        );
    }

    #[test]
    fn parse_rejects_extra_arguments() {
        assert!(parse("/status a b").is_err());
    }

    #[test]
    fn parse_rejects_unknown_command() {
        let err = parse("/frobnicate").unwrap_err();
        assert!(err.contains("unknown command"));
    }
}
