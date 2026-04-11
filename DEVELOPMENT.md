# Development Guide

## Project Overview

Mallard Metrics is a self-hosted, privacy-focused web analytics platform powered by DuckDB and the `behavioral` extension. Single binary, single process, zero external dependencies. Lightweight alternative to Plausible Analytics.

## Architecture

- **Language**: Rust (MSRV 1.94.0)
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

# Run all tests (333 total: 262 unit + 71 integration)
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
- **All tests pass** -- no ignored tests
- **Documentation builds without errors**
- Every claim in this file must be verifiable by running the relevant command

## Current Metrics

| Metric | Value | Verified |
|---|---|---|
| Unit tests | 262 | `cargo test --lib` |
| Integration tests | 71 | `cargo test --test ingest_test` |
| Total tests | 333 | `cargo test` |
| Clippy warnings | 0 | `cargo clippy --all-targets` |
| Format violations | 0 | `cargo fmt -- --check` |
| CI jobs | 12 | `.github/workflows/ci.yml` (4 jobs), `.github/workflows/pages.yml` (2 jobs), `.github/workflows/release.yml` (6 jobs) |

## Module Map

| Module | Purpose |
|---|---|
| `config.rs` | TOML + env var configuration |
| `server.rs` | Axum router setup |
| `ingest/handler.rs` | POST /api/event ingestion |
| `ingest/buffer.rs` | In-memory event buffer with periodic flush |
| `ingest/visitor_id.rs` | HMAC-SHA256 privacy-safe visitor ID |
| `ingest/useragent.rs` | User-Agent parsing |
| `ingest/geoip.rs` | MaxMind GeoIP reader with graceful fallback |
| `ingest/ratelimit.rs` | Per-site token-bucket rate limiter |
| `storage/schema.rs` | DuckDB table definitions |
| `storage/parquet.rs` | Parquet write/read/partitioning |
| `storage/migrations.rs` | Schema versioning |
| `query/metrics.rs` | Core metric calculations |
| `query/breakdowns.rs` | Dimension breakdown queries |
| `query/timeseries.rs` | Time-bucketed aggregations |
| `query/sessions.rs` | sessionize-based session queries |
| `query/funnel.rs` | window_funnel query builder |
| `query/retention.rs` | retention cohort query execution |
| `query/sequences.rs` | sequence_match query execution |
| `query/flow.rs` | sequence_next_node flow analysis |
| `query/cache.rs` | TTL-based query result cache |
| `api/stats.rs` | All analytics API handlers (core, sessions, funnel, retention, sequences, flow, export) |
| `api/errors.rs` | API error types |
| `api/auth.rs` | Origin validation, session auth, API key management |
| `dashboard/` | Embedded SPA (Preact + HTM) with 5 advanced analytics views |

## Development Workflow

1. Run the full validation suite before and after changes: `cargo test && cargo clippy --all-targets && cargo fmt -- --check`
2. Verify all claims with actual command output
3. Update documentation and test counts when adding features or tests

## Development Guidelines

- Do not claim performance numbers without Criterion measurement with confidence intervals
- Do not claim test counts without running `cargo test` and counting from output
- Do not guess SQL semantics -- test with actual DuckDB
- Do not introduce SQL injection (use parameterized queries)
- Do not store PII (IP addresses are used only for hashing, then discarded)
