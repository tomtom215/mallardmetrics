# Development Guide

## Project Overview

Mallard Metrics is a self-hosted, privacy-focused web analytics platform powered by DuckDB and the `behavioral` extension. Single binary, single process, zero external dependencies. Lightweight alternative to Plausible Analytics.

## Architecture

- **Language**: Rust, edition 2024 (MSRV 1.98.0)
- **Web framework**: Axum 0.8.x
- **Database**: DuckDB 1.5.5 (embedded, via the `duckdb` crate 1.10505.0 with `bundled` + `parquet` + `chrono` features)
- **Analytics**: `behavioral` community extension 0.9.1 (installed and loaded at runtime; published per DuckDB version, so the crate version is not a free choice)
- **Storage**: Two-tier — a DuckDB hot table plus date-partitioned Parquet, unioned by the `events_all` view
- **Frontend**: Preact + HTM (no build step, embedded in the binary)
- **Deployment**: Static musl binary in a distroless-style image, running as UID 65532

## Build & Test Commands

```bash
# Build
cargo build

# Run all tests (551 total: 489 unit + 62 integration)
cargo test --all-targets

# Behavioral-extension tests skip when the extension cannot be downloaded.
# Setting this turns a skip into a failure, which is what CI does.
MALLARD_REQUIRE_BEHAVIORAL=1 cargo test --all-targets

# End-to-end against the real binary on a real socket. The in-process tests
# cannot see a route that 500s only once the server is actually assembled.
cargo build && scripts/smoke-test.sh

# Clippy (zero warnings required)
cargo clippy --all-targets --all-features -- -D warnings

# Format check
cargo fmt -- --check

# Documentation
cargo doc --no-deps

# Run the server
cargo run

# Run benchmarks
cargo bench
```

## Quality Standards

- **Zero clippy warnings** (pedantic + nursery + cargo lint groups enabled)
- **Zero formatting violations**
- **All tests pass** -- no ignored tests
- **Documentation builds without errors**
- Every claim in this file must be verifiable by running the relevant command

## Current Metrics

| Metric | Value | Verified |
|---|---|---|
| Unit tests | 489 | `cargo test --lib` |
| Integration tests | 62 | `cargo test --test ingest_test` |
| Total tests | 551 | `cargo test --all-targets` |
| Property-test suites | 3 | `query/cache.rs`, `ingest/ratelimit.rs`, `ingest/visitor_id.rs` |
| Clippy warnings | 0 | `cargo clippy --all-targets --all-features -- -D warnings` |
| Format violations | 0 | `cargo fmt -- --check` |
| CI jobs | 16 | `.github/workflows/ci.yml` (8), `pages.yml` (2), `release.yml` (6) |

The `ci` job is a four-way matrix (Test, Clippy, Format, Documentation), so a
push runs 19 job instances from those 16 definitions.

## Module Map

See [`docs/src/architecture.md`](docs/src/architecture.md#module-map) for the
current map, kept in one place so the two cannot drift apart.

## Development Workflow

1. Run the full validation suite before and after changes:
   ```bash
   cargo test --all-targets \
     && cargo clippy --all-targets --all-features -- -D warnings \
     && cargo fmt -- --check \
     && cargo doc --no-deps
   ```
2. Verify all claims with actual command output
3. Update documentation and test counts when adding features or tests
4. `tracking/script.js` is served to every visitor on every page. The
   `static-checks` CI job holds it to a 4,096-byte gzipped budget (currently
   3,789); growing past that should be a decision, and the documented "about
   3 KB" updated with it

## Development Guidelines

- Do not claim performance numbers without Criterion measurement with confidence intervals
- Do not claim test counts without running `cargo test` and counting from output
- Do not guess SQL semantics -- test with actual DuckDB
- Do not introduce SQL injection (use parameterized queries)
- Do not store PII (IP addresses are used only for hashing, then discarded)
- Do not use SQL clock functions (`NOW()`, `CURRENT_TIMESTAMP`) against event
  data. They return `TIMESTAMP WITH TIME ZONE`, whose interval arithmetic lives
  in DuckDB's ICU extension, and whose cast to a naive `TIMESTAMP` follows the
  host's time zone — while every stored timestamp is naive UTC. Compute the
  instant in Rust with `Utc::now().naive_utc()` and bind it as a parameter.
