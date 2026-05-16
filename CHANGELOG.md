# Changelog

All notable changes to trusty-mpm are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
Versioning follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
