# Changelog

All notable changes to trusty-mpm are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-05-18

### Added
- **Session naming**: tmux sessions now use `tmpm-<folder>` derived from the project directory basename; daemon honours explicit `name` field in `POST /sessions`
- **Output style**: `trusty-mpm` Claude Code output style bundled as an asset and deployed to `~/.claude/output-styles/` on every `tm launch`; status bar shows `style:trusty-mpm`
- **Spinner tips**: 9 trusty-mpm-specific tips written to project `.claude/settings.json` on launch
- **Process tracking**: `find_claude_pid_in_tmux()` captures the `claude` PID after launch; reaper detects zombie state (tmux alive, claude dead) and marks sessions `Stopped`
- **Hook isolation**: `prepare_session()` writes trusty-memory hooks to project `.claude/settings.json` and removes them from global `~/.claude/settings.json`, preventing double-firing in claude-mpm sessions

### Changed
- tmux session naming scheme changed from random `tmpm-<adj>-<noun>` to deterministic `tmpm-<folder>` based on project directory basename

## [0.1.0] - 2026-05-17

### Added
- Single unified `trusty-mpm` binary covering daemon, TUI, CLI, and Telegram modes
- Full deterministic test coverage: 129 tests across all crates
- GitHub Actions CI/CD pipeline (test + lint + fmt on PRs; release builds + crates.io publish on tags)
- Makefile workflow targets (`make check`, `make install`, `make smoke`, `make pr`, ...)
- GitHub issue templates (bug, feature) and PR template
- Universal hook relay for all 32 Claude Code hook events
- Multi-session ratatui dashboard with circuit-breaker and event-feed panels
- Telegram remote-management bot with alert filtering
- Service discovery for trusty-memory and trusty-search sidecars
