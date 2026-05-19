.DEFAULT_GOAL := help

# ── Colours ──────────────────────────────────────────────────────────────────
BOLD  := $(shell tput bold 2>/dev/null)
RESET := $(shell tput sgr0 2>/dev/null)
GREEN := $(shell tput setaf 2 2>/dev/null)

.PHONY: help
help: ## Show this help
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) \
	  | awk 'BEGIN {FS = ":.*?## "}; {printf "$(BOLD)$(GREEN)%-20s$(RESET) %s\n", $$1, $$2}'

# ── Quality ───────────────────────────────────────────────────────────────────
.PHONY: check test lint fmt fmt-fix
check: test lint fmt  ## Run all quality gates (test + lint + fmt)

test:       ## Run the full test suite
	cargo test --workspace

lint:       ## Clippy with deny-warnings
	cargo clippy --workspace --all-targets -- -D warnings

fmt:        ## Check formatting
	cargo fmt --check

fmt-fix:    ## Apply formatting fixes
	cargo fmt

# ── Build ─────────────────────────────────────────────────────────────────────
.PHONY: build build-release
build:          ## Debug build
	cargo build --workspace

build-release:  ## Release build (lto + strip)
	cargo build --workspace --release

# ── Install / deploy ──────────────────────────────────────────────────────────
.PHONY: install uninstall
install:    ## Install tm and trusty-mpmd binaries
	cargo install --path crates/trusty-mpm-cli --locked
	cargo install --path crates/trusty-mpm-daemon --locked

uninstall:  ## Remove tm and trusty-mpmd from ~/.cargo/bin
	cargo uninstall trusty-mpm-cli
	cargo uninstall trusty-mpm-daemon

# ── Versioning (requires cargo-release) ──────────────────────────────────────
.PHONY: version-patch version-minor version-major
version-patch: ## Bump patch version (0.1.x → 0.1.x+1)
	cargo release version patch --execute

version-minor: ## Bump minor version (0.x.0 → 0.x+1.0)
	cargo release version minor --execute

version-major: ## Bump major version (x.0.0 → x+1.0.0)
	cargo release version major --execute

# ── GitHub flow ───────────────────────────────────────────────────────────────
.PHONY: issue pr
issue: ## Open a new GitHub issue interactively
	gh issue create

pr: check ## Run quality gate, then open a PR
	gh pr create

# ── Smoke / regression ────────────────────────────────────────────────────────
.PHONY: smoke
smoke: ## Run smoke tests against a live daemon (starts one if needed)
	@echo "$(BOLD)Starting daemon...$(RESET)"
	@trusty-mpm daemon &  DAEMON_PID=$$! ; \
	 sleep 1 ; \
	 echo "Probing /health..." ; \
	 curl -sf http://127.0.0.1:7880/health || (kill $$DAEMON_PID; exit 1) ; \
	 echo "Listing sessions..." ; \
	 curl -sf http://127.0.0.1:7880/sessions | python3 -m json.tool ; \
	 kill $$DAEMON_PID ; \
	 echo "$(GREEN)Smoke test passed$(RESET)"

# ── Publish ───────────────────────────────────────────────────────────────────
.PHONY: publish
publish: ## Publish all crates to crates.io (in dependency order)
	cargo publish -p trusty-mpm-core
	cargo publish -p trusty-mpm-mcp
	cargo publish -p trusty-mpm-daemon
	cargo publish -p trusty-mpm-client
	cargo publish -p trusty-mpm-tui
	cargo publish -p trusty-mpm-telegram
	cargo publish -p trusty-mpm-cli

# ── Dev shortcuts ─────────────────────────────────────────────────────────────
.PHONY: daemon tui
daemon: build ## Build and run the daemon (dev mode)
	./target/debug/trusty-mpm daemon

tui: build ## Build and run the TUI dashboard
	./target/debug/trusty-mpm tui
