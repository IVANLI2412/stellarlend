# StellarLend developer entry points.
# Thin wrappers around the existing `justfile` and npm workspace scripts:
# build logic stays in one place only.

.DEFAULT_GOAL := help
.PHONY: help dev fmt check lint test ci devnet local-net local-net-down clean

help: ## List available targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  %-18s %s\n", $$1, $$2}'

dev: ## Bootstrap dev environment (prereqs + workspace install)
	@bash scripts/dev.sh

fmt: ## Format Rust and check TypeScript formatting
	@just fmt
	@npx prettier --check "test/**/*.ts"

check: ## Type-check Rust workspace and TypeScript packages
	@just check
	@npm run typecheck

lint: ## Lint Rust with clippy and all npm workspaces
	@just clippy
	@npm run lint:all

test: ## Run Rust workspace tests and all npm workspace tests
	@just test
	@npm run test:all

ci: ## Run the full local CI suite
	@just ci

devnet: ## Bootstrap the local Stellar devnet
	@bash scripts/devnet/bootstrap.sh

local-net: ## Start local docker network
	@just local-net

local-net-down: ## Stop local docker network
	@just local-net-down

clean: ## Remove build artifacts
	@cargo clean --manifest-path stellar-lend/Cargo.toml
	@rm -rf node_modules test-results coverage
