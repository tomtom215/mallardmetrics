# CLAUDE.md — Mallard Metrics

## Project Overview

Mallard Metrics is a self-hosted, privacy-focused web analytics platform powered by DuckDB and the `behavioral` extension. Single binary, single process, zero external dependencies. Lightweight alternative to Plausible Analytics.

## Architecture

- **Language**: Rust (MSRV 1.93.0)
- **Web framework**: Axum 0.8.x
- **Database**: DuckDB (embedded, via `duckdb` crate with `bundled` + `parquet` features)
- **Analytics**: `behavioral` extension (loaded at runtime)
- **Storage**: Date-partitioned Parquet files
- **Frontend**: Preact + HTM (no build step, embedded in binary)
- **Deployment**: Static musl binary, `FROM scratch` Docker image

## Build & Test Commands

```bash
# Build
cargo build

# Run all tests (111 total: 104 unit + 7 integration)
cargo test

# Clippy (zero warnings required)
cargo clippy --all-targets

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
- **All tests pass** — no ignored tests
- **Documentation builds without errors**
- Every claim in this file must be verifiable by running the relevant command

## Current Metrics

| Metric | Value | Verified |
|---|---|---|
| Unit tests | 104 | `cargo test --lib` |
| Integration tests | 7 | `cargo test --test ingest_test` |
| Total tests | 111 | `cargo test` |
| Clippy warnings | 0 | `cargo clippy --all-targets` |
| Format violations | 0 | `cargo fmt -- --check` |
| CI jobs | 11 | `.github/workflows/ci.yml` |

## Module Map

| Module | Purpose |
|---|---|
| `config.rs` | TOML + env var configuration |
| `server.rs` | Axum router setup |
| `ingest/handler.rs` | POST /api/event ingestion |
| `ingest/buffer.rs` | In-memory event buffer with periodic flush |
| `ingest/visitor_id.rs` | HMAC-SHA256 privacy-safe visitor ID |
| `ingest/useragent.rs` | User-Agent parsing |
| `ingest/geoip.rs` | GeoIP stub (Phase 4) |
| `storage/schema.rs` | DuckDB table definitions |
| `storage/parquet.rs` | Parquet write/read/partitioning |
| `storage/migrations.rs` | Schema versioning |
| `query/metrics.rs` | Core metric calculations |
| `query/breakdowns.rs` | Dimension breakdown queries |
| `query/timeseries.rs` | Time-bucketed aggregations |
| `query/sessions.rs` | sessionize-based session queries |
| `query/funnel.rs` | window_funnel query builder |
| `query/retention.rs` | retention query builder |
| `query/sequences.rs` | sequence_match/count query builder |
| `query/flow.rs` | sequence_next_node query builder |
| `api/stats.rs` | Dashboard API handlers |
| `api/errors.rs` | API error types |
| `api/auth.rs` | Authentication (stub for Phase 1) |
| `dashboard/` | Embedded SPA (Preact + HTM) |

## Session Protocol

1. Read CLAUDE.md and LESSONS.md before starting work
2. Run `cargo test && cargo clippy --all-targets && cargo fmt -- --check` at start
3. Document all changes with verifiable before/after evidence
4. Run the full validation suite at the end
5. Update CLAUDE.md with session entry
6. Verify every claim

## Anti-Patterns

- Do NOT claim performance numbers without Criterion measurement with CIs
- Do NOT claim test counts without running `cargo test` and counting from output
- Do NOT add session entries for work not completed and verified
- Do NOT guess SQL semantics — test with actual DuckDB
- Do NOT introduce SQL injection (use parameterized queries)
- Do NOT store PII (IP addresses only for hashing, then discarded)

## Session Log

### Session 1: Project Initialization (Phase 1)

**Changes:**
- Initialized Rust project with Cargo.toml, rust-toolchain.toml (1.85.0), deny.toml
- Implemented full ingestion pipeline: POST /api/event → buffer → Parquet flush
- Implemented privacy-safe visitor ID (HMAC-SHA256 with daily salt rotation)
- Implemented date-partitioned Parquet storage
- Created DuckDB schema, migrations, and behavioral extension loading
- Built Axum HTTP server with health check, ingestion, stats, and breakdown endpoints
- Created dashboard SPA (Preact + HTM, embedded via rust-embed)
- Created tracking script (<1KB)
- Created CI pipeline (11 jobs: build, test, clippy, fmt, docs, MSRV, bench, security, coverage, docker, cross-compile)
- All GitHub Actions pinned to commit SHAs

**Test results:**
- 104 unit tests passing
- 7 integration tests passing (HTTP API tests)
- 0 clippy warnings
- 0 formatting violations

**Test categories implemented:**
- Config loading (defaults, TOML file, env var overrides, invalid input)
- Visitor ID (determinism, uniqueness, daily salt rotation)
- Event buffer (push, threshold flush, manual flush, multi-site)
- Parquet storage (partitioning, flush, incremental numbering, multi-site, multi-date)
- Schema (creation, idempotency, column verification)
- Migrations (fresh DB, idempotency)
- HTTP API (health check, event ingestion, validation, stats, breakdowns, CORS)
- User-Agent parsing (Chrome, Firefox, Safari, Edge, Android, iPhone)
- Referrer source detection (Google, Twitter, Facebook, Reddit, etc.)
- UTM parameter parsing
- Query metrics (unique visitors, pageviews, date ranges)
- Breakdowns (by page, browser, with limits, null handling)
- Timeseries (daily, hourly)
