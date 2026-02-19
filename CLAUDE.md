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

# Run all tests (222 total: 179 unit + 43 integration)
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
| Unit tests | 183 | `cargo test --lib` |
| Integration tests | 43 | `cargo test --test ingest_test` |
| Total tests | 226 | `cargo test` |
| Clippy warnings | 0 | `cargo clippy --all-targets` |
| Format violations | 0 | `cargo fmt -- --check` |
| CI jobs | 11 | `.github/workflows/ci.yml`, `.github/workflows/pages.yml` |

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
- Created CI pipeline (10 jobs: build, test, clippy, fmt, docs, MSRV, bench, security, coverage, docker)
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

### Session 2: Phase 2 — Dashboard & Integration Fixes

**Changes:**
- **Task 2.1:** Integrated UA parser into ingestion handler — `parse_user_agent()` now called from `ingest_event()`, replacing hardcoded `None` values for browser, browser_version, os, os_version fields (`handler.rs:93-94`)
- **Task 2.2:** Integrated GeoIP stub into ingestion handler — `geoip::lookup()` now called from `ingest_event()`, replacing hardcoded `None` values for country_code, region, city fields (`handler.rs:97`). Returns `None` for all fields until Phase 4 MaxMind integration.
- **Task 2.3:** Added timeseries line chart to dashboard — SVG-based chart component renders visitors (solid blue) and pageviews (dashed green) lines using existing `/api/stats/timeseries` endpoint. Zero external dependencies.
- **Task 2.4:** Added all 6 breakdown tables to dashboard — Pages, Sources, Browsers, OS, Devices, and Countries breakdowns fetched via `Promise.all()` from existing API endpoints. Displayed in responsive grid layout.
- **Task 2.5:** Enhanced tracking script with custom events and revenue support — exposed `window.mallard(eventName, options)` public API. Supports `props` (custom properties), `revenue`, `currency`, and `callback` options. Script size: 774 bytes (under 1KB constraint).
- **Task 2.6:** Integrated origin validation into ingestion handler — `validate_origin()` now called from `ingest_event()` using `allowed_sites` from config. Returns 403 Forbidden for disallowed origins. Empty `site_ids` config allows all origins (default behavior).
- Removed all `#[allow(dead_code)]` annotations from newly-wired code: `parse_user_agent`, `ParsedUserAgent`, `detect_browser`, `detect_browser_version`, `detect_os`, `detect_os_version`, `extract_version_after`, `GeoInfo`, `geoip::lookup`, `validate_origin`, `Config.site_ids`
- Added `allowed_sites` field to `AppState` struct

**Test results:**
- 104 unit tests passing (`cargo test --lib`)
- 12 integration tests passing (`cargo test --test ingest_test`)
- Total: 116 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)

**New integration tests added (5):**
- `test_ua_parsing_populates_browser_os_fields` — verifies Chrome/Windows UA produces correct browser/OS fields in Parquet
- `test_ua_parsing_firefox_on_linux` — verifies Firefox/Linux UA produces correct fields
- `test_origin_validation_rejects_disallowed_origin` — verifies 403 for non-matching origin
- `test_origin_validation_allows_matching_origin` — verifies 202 for matching origin
- `test_origin_validation_allows_no_origin_header` — verifies 202 when no Origin header (server-side requests)

### Session 3: Phase 3 — Behavioral Analytics (Advanced Queries)

**Changes:**
- **Task 3.1:** Wired session analytics — `query_session_metrics()` now exposed via `GET /api/stats/sessions`. Removed `#[allow(dead_code)]` from `sessions.rs`. Handler returns graceful defaults (all zeros) when behavioral extension is unavailable. Dashboard shows session cards (total sessions, avg duration, pages/session).
- **Task 3.2:** Replaced funnel 501 stub — `GET /api/stats/funnel` now accepts `steps` parameter in safe `page:/path` or `event:name` format, parsed by `parse_funnel_step()` which prevents SQL injection. Validates `window` interval via `is_safe_interval()`. Removed `funnels.rs` stub; handler moved to `stats.rs`. Dashboard shows horizontal bar funnel visualization with configurable steps.
- **Task 3.3:** Refactored retention from SQL-string-builder to executor — `query_retention()` now returns `Result<Vec<RetentionCohort>>` instead of `String`. Executes the SQL with parameterized site_id/dates. Parses DuckDB `BOOLEAN[]` array output via `parse_bool_array()`. Uses `STRFTIME` for consistent date formatting (L6). Dashboard shows retention cohort grid table.
- **Task 3.4:** Refactored sequences from SQL-string-builders to executor — Replaced `build_sequence_match_sql(pattern, conditions)` and `build_sequence_count_sql` with `execute_sequence_match()` that builds safe patterns from condition count (`(?1).*(?2)`) instead of accepting raw pattern strings. API accepts safe `page:/event:` step format (same as funnel). Dashboard shows conversion metrics cards.
- **Task 3.5:** Fixed flow analysis SQL injection — `build_flow_sql(target_page)` replaced with `query_flow()` that escapes single quotes in `target_page` via `replace('\'', "''")`. API validates page path length. Added `FlowNode` result struct. Dashboard shows next-page table.
- Removed `api/funnels.rs` module (stub replaced)
- Removed all `#[allow(dead_code)]` from: `SessionMetrics`, `query_session_metrics`, `query_funnel`, `RetentionCohort`, `SequenceMatchResult`, `FlowNode`
- All 5 new routes registered in `server.rs`: `/api/stats/sessions`, `/api/stats/funnel`, `/api/stats/retention`, `/api/stats/sequences`, `/api/stats/flow`
- All features degrade gracefully without behavioral extension (return defaults/empty results)

**Test results:**
- 116 unit tests passing (`cargo test --lib`)
- 22 integration tests passing (`cargo test --test ingest_test`)
- Total: 138 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)
- Documentation builds without errors (`cargo doc --no-deps`)

**New unit tests added (12):**
- `test_parse_funnel_step_page` — validates `page:/pricing` → `pathname = '/pricing'`
- `test_parse_funnel_step_event` — validates `event:signup` → `event_name = 'signup'`
- `test_parse_funnel_step_escapes_quotes` — validates single-quote escaping
- `test_parse_funnel_step_invalid_format` — rejects arbitrary strings
- `test_is_safe_interval_valid` — validates safe interval formats
- `test_is_safe_interval_invalid` — rejects injection attempts in intervals
- `test_retention_zero_weeks` — validates early return for zero weeks
- `test_parse_bool_array` — validates DuckDB BOOLEAN[] parsing
- `test_build_pattern` — validates sequence pattern generation
- `test_execute_empty_conditions` — validates early return for empty sequences
- `test_query_flow_escapes_quotes` — validates SQL injection prevention in flow
- `test_session_metrics_with_data_no_extension` — validates graceful degradation

**New integration tests added (10):**
- `test_sessions_endpoint_returns_ok` — verifies 200 with session metric fields
- `test_funnel_endpoint_with_valid_steps` — verifies 200 with valid step format
- `test_funnel_endpoint_rejects_invalid_steps` — verifies 400 for SQL injection attempts
- `test_funnel_endpoint_rejects_invalid_window` — verifies 400 for malicious window intervals
- `test_retention_endpoint_returns_ok` — verifies 200 for retention query
- `test_retention_endpoint_rejects_invalid_weeks` — verifies 400 for weeks=0
- `test_sequences_endpoint_returns_ok` — verifies 200 with conversion metrics
- `test_sequences_endpoint_requires_two_steps` — verifies 400 for single step
- `test_flow_endpoint_returns_ok` — verifies 200 for flow analysis
- `test_flow_endpoint_rejects_empty_page` — verifies 400 for empty page path

### Session 4: Phase 4 — Production Hardening

**Changes:**
- Implemented Argon2id password hashing for dashboard authentication
- Added session management with 256-bit cryptographic tokens (HttpOnly cookies)
- Added API key management (CRUD endpoints, SHA-256 hashed at rest, `mm_` prefix)
- Added auth middleware protecting stats and key management routes
- Added CORS hardening: permissive for ingestion, restrictive for dashboard
- Added MaxMind GeoLite2 GeoIP reader with graceful fallback
- Added bot traffic filtering via User-Agent detection

**Test results:**
- 146 unit tests, 38 integration tests, total 184
- 0 clippy warnings, 0 format violations

### Session 5: Phases 5+6 — Operational Excellence & Scale

**Changes:**
- **Phase 5.1:** Data retention cleanup — `cleanup_old_partitions()` removes Parquet files older than configured retention period, runs daily
- **Phase 5.2:** Data export API — `GET /api/stats/export` returns CSV or JSON format with daily visitor/pageview data
- **Phase 5.3:** Graceful shutdown — SIGINT/SIGTERM handling with buffered event flush before exit
- **Phase 5.4:** Enhanced health check — `GET /health/detailed` returns JSON with version, buffer, auth, geoip, cache status
- **Phase 5.5:** Structured logging — `MALLARD_LOG_FORMAT=json` enables JSON-formatted tracing output
- **Phase 5.6:** Config template — `mallard-metrics.toml.example` with all options documented
- **Phase 5.7:** Docker optimization — Dependency caching layer in Dockerfile, enhanced docker-compose with restart policy
- **Phase 6.1:** Query result caching — TTL-based in-memory cache for main stats and timeseries endpoints
- **Phase 6.2:** Rate limiting — Per-site token-bucket rate limiter for ingestion endpoint (configurable via `rate_limit_per_site`)
- **Phase 6.4:** Benchmark suite — Added query benchmarks (core metrics, timeseries, breakdowns) to Criterion suite
- **Phase 6.5:** Prometheus metrics — `GET /metrics` endpoint with `text/plain; version=0.0.4` format

**New modules:**
- `query/cache.rs` — Thread-safe query cache with TTL expiration
- `ingest/ratelimit.rs` — Token-bucket rate limiter per site_id

**Test results:**
- 166 unit tests, 43 integration tests, total 209
- 0 clippy warnings, 0 format violations
- Documentation builds without errors

### Session 6: Comprehensive Project Audit

**Scope:** Full codebase audit covering all Rust source files, tests, documentation, CI/CD, and frontend code.

**Security fixes:**
- **CSV injection prevention** (`api/stats.rs`) — Added `escape_csv_field()` that prefixes formula-triggering characters (`=`, `+`, `-`, `@`) with a single quote in CSV export output
- **Constant-time API key comparison** (`api/auth.rs`) — Added `constant_time_eq()` using XOR accumulation to prevent timing side-channel attacks in `validate_key()`
- **Path traversal prevention** (`storage/parquet.rs`) — Added `is_safe_path_component()` validation rejecting `..`, `/`, `\`, null bytes, and empty strings in site_id before filesystem operations

**Code quality fixes:**
- **Export format validation** (`api/stats.rs`) — Replaced `if/else` with `match` on format parameter; invalid formats now return 400 with descriptive message instead of silently defaulting to CSV
- **JSON serialization error handling** (`api/stats.rs`) — Replaced `unwrap_or_default()` with proper error propagation via `ApiError::Internal` for JSON serialization failures

**New unit tests added (13):**
- `test_escape_csv_field_plain` — plain text passes through unchanged
- `test_escape_csv_field_with_quotes` — double quotes are escaped
- `test_escape_csv_field_formula_injection` — formula characters get single-quote prefix
- `test_export_invalid_format` — invalid format returns BadRequest
- `test_constant_time_eq_equal` — matching byte slices return true
- `test_constant_time_eq_not_equal` — differing byte slices return false
- `test_constant_time_eq_different_lengths` — length mismatch returns false
- `test_constant_time_eq_empty` — empty slices return true
- `test_is_safe_path_component_valid` — normal site_ids accepted
- `test_is_safe_path_component_rejects_traversal` — `..` rejected
- `test_is_safe_path_component_rejects_slashes` — `/` and `\` rejected
- `test_is_safe_path_component_rejects_empty` — empty string rejected
- `test_is_safe_path_component_rejects_null` — null bytes rejected

**Test results:**
- 179 unit tests, 43 integration tests, total 222
- 0 clippy warnings, 0 format violations

### Session 7: Enterprise Code Review & GitHub Pages Documentation

**Scope:** Full peer review of all source files, security findings fixed, and GitHub Pages documentation site created.

**Security fixes:**
- **`validate_origin` prefix-bypass (CRITICAL)** (`api/auth.rs`) — `host.starts_with(s)` allowed `example.com.evil.com` to bypass an allowlist containing `"example.com"`. Fixed: extract the authority (host[:port]) by splitting on `/` after stripping the scheme, then use exact equality or explicit port-suffix check. Added 2 regression tests.

**Correctness fixes:**
- **`shutdown_timeout_secs` not enforced** (`main.rs`) — The timeout was logged but not used. Fixed: flush is now wrapped in `tokio::time::timeout(Duration::from_secs(timeout_secs))` via `spawn_blocking`; a `WARN` log is emitted if the flush does not complete in time.
- **Parquet query gap (CRITICAL)** — Events written to Parquet were deleted from the DuckDB in-memory `events` table, making all flushed data invisible to analytics queries. Fixed: added `storage::schema::setup_query_view(conn, parquet_dir)` which creates an `events_all` DuckDB VIEW unioning the hot `events` table with a `read_parquet()` glob over all persisted files. All 11 query sites across 8 modules now target `events_all`. The view is refreshed at startup (for restart recovery) and after each `flush_events()` call (so new Parquet files are immediately queryable). Added 1 new unit test (`test_setup_query_view_no_parquet`).

**Documentation:**
- Created GitHub Pages documentation site using mdBook (`docs/`):
  - `docs/book.toml` — mdBook configuration (mdBook v0.4.40)
  - `docs/src/SUMMARY.md` — Table of contents (13 pages)
  - Introduction, Quick Start, Tracking Script, Configuration, API Reference (4 sub-pages), Architecture, Security & Privacy, Behavioral Analytics, Deployment, Monitoring, Data Management
  - `docs/src/custom.css` — Brand-consistent styling
- Created `.github/workflows/pages.yml` — Deploys to GitHub Pages on push to `main` using SHA-pinned actions.

**Test results:**
- 183 unit tests passing (`cargo test --lib`)
- 43 integration tests passing (`cargo test --test ingest_test`)
- Total: 226 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)

**New unit tests added (4):**
- `test_validate_origin_with_port` — validates `http://example.com:3000` against `"example.com"`
- `test_validate_origin_prefix_bypass_rejected` — `https://example.com.evil.com` must NOT match `"example.com"`
- `test_validate_origin_prefix_subdomain_bypass_rejected` — `https://example.com-other.io` must NOT match `"example.com"`
- `test_setup_query_view_no_parquet` — `events_all` view is queryable even with no Parquet files present
