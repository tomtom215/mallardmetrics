# Developer shortcuts. Every target is a thin wrapper over the commands in
# CONTRIBUTING.md and DEVELOPMENT.md; nothing here is required to build.
#
#   make help
#
# Builds share one target directory so concurrent worktrees do not each compile
# DuckDB: export CARGO_TARGET_DIR=$HOME/mallard-target (or similar) first.

.DEFAULT_GOAL := help
.PHONY: help check fmt lint test test-behavioral doc smoke run docker docker-smoke shellcheck seed

help: ## List the targets
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F':.*## ' '{printf "  %-16s %s\n", $$1, $$2}'

check: fmt lint test doc ## Everything CI's Rust jobs run (the validation suite)

fmt: ## Verify formatting
	cargo fmt -- --check

lint: ## Clippy, warnings are errors
	cargo clippy --locked --all-targets --all-features -- -D warnings

test: ## All tests (behavioral ones skip when the extension cannot be downloaded)
	cargo test --locked --all-targets

test-behavioral: ## All tests, failing instead of skipping when the extension is missing
	MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --locked --all-targets

doc: ## Rustdoc with warnings as errors
	RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps

smoke: ## End-to-end checks against the real binary, extension required
	cargo build --locked
	MALLARD_REQUIRE_BEHAVIORAL=1 scripts/smoke-test.sh $${CARGO_TARGET_DIR:-target}/debug/mallard-metrics

run: ## Run the server with a throwaway data directory
	MALLARD_DATA_DIR=$${TMPDIR:-/tmp}/mallard-dev-data cargo run

docker: ## Build the container image (compiles DuckDB: slow the first time)
	docker build -t mallard-metrics:test .

docker-smoke: docker ## Build the image and run it with the production restrictions
	scripts/docker-smoke.sh mallard-metrics:test

shellcheck: ## Lint the shell scripts
	shellcheck scripts/*.sh deploy/*.sh

seed: ## Write 30 days of synthetic events (needs: pip install duckdb)
	python3 scripts/seed-demo-data.py
