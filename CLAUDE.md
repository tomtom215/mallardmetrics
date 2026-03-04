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
- **All tests pass** — no ignored tests
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
| CI jobs | 12 | `.github/workflows/ci.yml` (10 jobs), `.github/workflows/pages.yml` (2 jobs) |

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

### Session 8: Full Code Review & Audit Remediation

**Scope:** Comprehensive peer-review audit of all Rust source, tests, documentation, CI/CD, and frontend code.

**Code fixes:**
- **`api/errors.rs` — removed dead `Unauthorized` variant** with `#[allow(dead_code)]`. The variant was never constructed in production code and violated YAGNI. The matching `Display` and `IntoResponse` arms were removed. `test_not_found_status` is preserved (it tests the `NotFound` variant which is now used in production via `revoke_api_key_handler`).
- **`api/auth.rs` — `revoke_api_key_handler` uses `ApiError::NotFound`** — Changed return type from `impl IntoResponse` to `Result<impl IntoResponse, ApiError>` and returns `ApiError::NotFound("Key not found")` instead of a raw `(StatusCode::NOT_FOUND, Json(...))` tuple. This is consistent with all other error handling in the codebase.
- **`server.rs` — fixed misleading CORS comment** — Comment on the permissive dashboard CORS branch previously said "same-origin requests work by default", implying same-origin-only behavior. Corrected to explicitly state it allows all origins and to set `dashboard_origin` to restrict access.
- **`storage/parquet.rs` — `next_file_path` no longer silently discards `create_dir_all` errors** — Replaced `.ok()` with an explicit `tracing::warn!` log. The partition path is still returned (the subsequent `COPY TO` will produce the definitive error), but the operator now sees the root cause in logs.
- **`dashboard/assets/app.js` — eliminated dead ternary** — `d.date.length > 10 ? d.date.slice(5) : d.date.slice(5)` had identical then/else branches. Simplified to `d.date.slice(5)`.
- **`dashboard/assets/app.js` — added authentication flow** — The dashboard had no auth UI: a password-protected instance would show an unhelpful "HTTP 401" error with no way to sign in. Added:
  - `App` shell component that calls `GET /api/auth/status` on mount.
  - `LoginForm` component for sign-in (password configured) and first-run setup (no password yet).
  - `Dashboard` now accepts `onLogout` and `onAuthExpired` props; shows "Sign Out" button when a password is configured; re-shows `LoginForm` on session expiry.
  - Auth form styles added to `style.css` (`.auth-overlay`, `.auth-card`, `.btn-logout`, `.loading-screen`).
- **`dashboard/assets/app.js` — corrected `SessionCards` field names** — Component was reading `data.avg_duration_secs` and `data.pages_per_session`, which do not exist in the API response. Corrected to `data.avg_session_duration_secs` and `data.avg_pages_per_session` (matching `SessionMetrics` in `sessions.rs`).

**Documentation fixes (22 issues corrected):**
- `docs/src/api-reference/stats.md` — Period values corrected (`24h`, `12mo`, `all` → `day`, `today`, `7d`, `30d`, `90d`).
- `docs/src/api-reference/stats.md` — Timeseries response field `bucket` → `date`.
- `docs/src/api-reference/stats.md` — Sessions response field names corrected (`avg_duration_secs` → `avg_session_duration_secs`, `pages_per_session` → `avg_pages_per_session`).
- `docs/src/api-reference/stats.md` — Export CSV format corrected from 3-column to 5-column (`date,visitors,pageviews,top_page,top_source`).
- `docs/src/api-reference/stats.md` — Export JSON corrected from 3-field to 5-field response.
- `docs/src/api-reference/stats.md` — Breakdown default limit corrected from 50 → 10.
- `docs/src/api-reference/stats.md` — Funnel `steps` format corrected from repeated query params to comma-separated value.
- `docs/src/api-reference/stats.md` — Removed fake `interval` parameter from timeseries (granularity is auto-determined from `period`).
- `docs/src/api-reference/auth.md` — Login/setup response format corrected (`{"message": "..."}` → `{"token": "..."}`).
- `docs/src/api-reference/auth.md` — Logout response format corrected (`{"message": "Logged out"}` → `{"status": "logged_out"}`).
- `docs/src/api-reference/auth.md` — Auth status response fields corrected (`has_password` removed, `setup_required` added).
- `docs/src/api-reference/auth.md` — Revoke key response corrected (204 No Content → 200 `{"status": "revoked"}` / 404 `{"error": "Key not found"}`).
- `docs/src/api-reference/auth.md` — `ApiKeyScope` serialization corrected (`"read_only"` → `"ReadOnly"`; `Admin` scope documented).
- `docs/src/api-reference/auth.md` — `key_hash` format corrected (removed non-existent `"sha256:"` prefix).
- `docs/src/api-reference/ingestion.md` — Revenue field names corrected (`"$"` → `"ra"`, `"c"` → `"rc"`); `props` type corrected (JSON-encoded string, not object).
- `docs/src/api-reference/index.md` — Removed non-existent `"code"` field from error response examples.
- `docs/src/behavioral-analytics.md` — Sessions field names corrected to match API.
- `docs/src/behavioral-analytics.md` — Funnel example URL corrected to comma-separated `steps` format.
- `docs/src/behavioral-analytics.md` — Sequences example URL corrected to comma-separated format.
- `docs/src/behavioral-analytics.md` — "Retention — Cohort grid table with percentage overlays" corrected (no percentages; cells show Y / - booleans).
- `docs/src/security.md` — Visitor ID algorithm documented accurately (two-step HMAC-SHA256 derivation with intermediate `daily_salt`; MALLARD_SECRET is an input to the intermediate hash, not the outer HMAC key).
- `docs/src/security.md` — CORS comment corrected: permissive dashboard CORS explicitly allows all origins (not "same-origin only").
- `README.md` — Test count corrected (209 → 226; 166 unit → 183 unit).
- `README.md` — Architecture diagram redrawn to accurately show two-tier DuckDB/Parquet storage model and `events_all` VIEW.
- `CLAUDE.md` — CI job count corrected (11 → 12).
- `CLAUDE.md` — Test count comment corrected (222 → 226).

**Test results:**
- 183 unit tests passing (`cargo test --lib`)
- 43 integration tests passing (`cargo test --test ingest_test`)
- Total: 226 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)

### Session 10: Production-Readiness Audit Remediation

**Scope:** All 25 production-readiness gaps from external audit — BLOCKING (5), HIGH (7), MEDIUM (8), LOW (5).

**Security fixes:**
- Brute-force protection: `LoginAttemptTracker` (per-IP attempt counting, configurable lockout via `MALLARD_MAX_LOGIN_ATTEMPTS` / `MALLARD_LOGIN_LOCKOUT`). Returns 429 after `max_attempts` failures from same IP. Lock cleared on success.
- Body size limit: `DefaultBodyLimit::max(65_536)` on ingestion routes; 413 on overflow.
- OWASP security headers: `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: strict-origin-when-cross-origin`, `Content-Security-Policy` (HTML only) injected via `add_security_headers` `map_response` middleware.
- HTTP timeout: `TimeoutLayer::with_status_code(REQUEST_TIMEOUT, 30s)` on router.
- CSRF protection: `validate_csrf_origin()` checks Origin/Referer on all session-auth state-mutating routes.
- API key scope enforcement: `require_admin_auth` middleware returns 403 for `ReadOnly` keys on admin-only routes. `X-API-Key` header supported alongside `Authorization: Bearer`.
- IP audit logging: `tracing::warn!/info!` on login failures, lockouts, setup, logout, key operations. IPs anonymized (last IPv4 octet masked, IPv6 truncated).

**Completeness fixes:**
- Prometheus counter: `mallard_events_ingested_total` (`AtomicU64`) wired end-to-end.
- Config validation: `Config::validate()` exits 1 at startup for invalid config fields.
- `site_id` validation: `validate_site_id()` rejects empty, >256 chars, disallowed chars on all stats endpoints.
- Revoked key GC: `api_keys.cleanup_revoked()` runs in 15-minute background task.
- Dashboard: CSV + JSON export download buttons added.
- Dashboard: Funnel chart division-by-zero guard added.
- Local JS: `preact.js` + `htm.js` bundled via `rust-embed`; CDN dependency eliminated.

**Test results:**
- 209 unit tests passing (`cargo test --lib`)
- 56 integration tests passing (`cargo test --test ingest_test`)
- Total: 265 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)

**New unit tests added (26):**
- `test_login_tracker_allows_below_limit` — per-IP tracker permits requests below max
- `test_login_tracker_lockout_after_max_attempts` — returns 429 after threshold
- `test_login_tracker_success_clears_failures` — successful login resets failure count
- `test_login_tracker_independent_ips` — different IPs do not share lockout state
- `test_login_tracker_disabled_when_max_zero` — max_attempts=0 disables protection
- `test_csrf_validate_matching_origin_allowed` — matching Origin passes CSRF check
- `test_csrf_validate_mismatching_origin_rejected` — mismatched Origin rejected
- `test_csrf_validate_no_dashboard_origin_allows_all` — no dashboard_origin bypasses CSRF
- `test_csrf_validate_no_origin_or_referer_allows` — server-side requests without Origin pass
- `test_api_key_store_scope_distinction` — ReadOnly and Admin keys are distinct scopes
- `test_api_key_store_cleanup_revoked` — revoked keys are removed by cleanup
- `test_validate_site_id_valid` — normal site_ids accepted
- `test_validate_site_id_empty` — empty string rejected
- `test_validate_site_id_too_long` — >256 chars rejected
- `test_validate_site_id_invalid_chars` — disallowed characters rejected
- `test_validate_valid_config` — valid config passes validation
- `test_validate_zero_flush_count` — flush_count=0 rejected at startup
- `test_validate_zero_flush_interval` — flush_interval=0 rejected at startup
- `test_validate_zero_session_ttl` — session_ttl=0 rejected at startup
- `test_cors_headers` — CORS response headers present on OPTIONS request
- `test_dashboard_index` — dashboard HTML served correctly
- `test_classify_device_desktop` — desktop UA classified correctly
- `test_classify_device_mobile` — mobile UA classified correctly
- `test_classify_device_tablet` — tablet UA classified correctly
- `test_extract_ip_from_x_forwarded_for` — real IP extracted from X-Forwarded-For
- `test_extract_ip_from_x_real_ip` — real IP extracted from X-Real-Ip

**New integration tests added (13):**
- `test_login_rate_limited_after_failures` — 429 returned after max attempts
- `test_login_success_clears_failure_count` — lock cleared on valid credential
- `test_ingest_rejects_oversized_body` — 413 returned for body >65 536 bytes
- `test_security_headers_present` — OWASP headers verified on response
- `test_events_ingested_counter` — events_ingested counter increments on ingest
- `test_prometheus_metrics_includes_counter` — /metrics includes the counter
- `test_api_key_scope_readonly_cannot_create_key` — ReadOnly key rejected on admin route
- `test_api_key_scope_admin_can_create_key` — Admin key accepted on admin route
- `test_x_api_key_header_authentication` — X-API-Key header auth works
- `test_stats_invalid_site_id_rejected` — invalid site_id returns 400
- `test_stats_empty_site_id_rejected` — empty site_id returns 400
- `test_timeseries_invalid_period_rejected` — invalid period returns 400
- `test_data_persists_after_view_rebuild` — events_all VIEW rebuilt correctly after restart

### Session 9: Documentation Audit — Stale Test Counts

**Scope:** Cross-file documentation audit for consistency with the codebase state established in Sessions 5–8.

**Documentation fixes (2 issues corrected):**
- **`CONTRIBUTING.md` — stale test count** — Validation suite table listed "All 209 tests pass (166 unit + 43 integration)". Corrected to "226 tests pass (183 unit + 43 integration)" to match the actual test suite, which grew from 209 to 226 across Sessions 6–7 (13 new unit tests in Session 6, 4 new unit tests in Session 7).
- **`ROADMAP.md` — stale test count** — Implementation Summary status overview listed "209 tests (166 unit + 43 integration)". Corrected to "226 tests (183 unit + 43 integration)".

**Test results:**
- 183 unit tests passing (`cargo test --lib`)
- 43 integration tests passing (`cargo test --test ingest_test`)
- Total: 226 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)
- Documentation builds without errors (`cargo doc --no-deps`)

### Session 11: Documentation Sync, Benchmark Baseline, Property Tests

**Scope:** T1 (doc test-count sync), T2 (Criterion baseline), T3 (proptest), T6 (LESSONS.md L16–L18).

**Changes:**

- **T1: Documentation sync** — Corrected stale test counts in README.md, CONTRIBUTING.md, CLAUDE.md, and ROADMAP.md. Prior session (10) left counts at 265 (209 unit + 56 integration); actual counts at session start were 219 unit + 61 integration = 280 total (verified by `cargo test --lib` and `cargo test --test ingest_test`). All four files updated.

- **T2: Criterion benchmark baseline** — Established first formal baseline in PERF.md "Current Baseline" section (previously "Not yet published"). Three runs completed for `ingest_throughput` (100/1K/10K) and `query_metrics` (core_metrics, timeseries, breakdown_pages). One run completed for `parquet_flush/1000` (6.04 s per iteration; 100-sample run = 612 s). `parquet_flush/10000` not measured (Criterion estimated 6027 s per run; impractical for 3× in a session). Environment: rustc 1.93.1, Linux 4.4.0, Intel 2.1 GHz, 16 cores, 21 GiB RAM. Canonical values (median run):
  - `ingest_throughput/100`: 17.265 ms [16.964 ms, 17.582 ms]
  - `ingest_throughput/1000`: 19.794 ms [19.317 ms, 20.264 ms]
  - `ingest_throughput/10000`: 29.715 ms [28.726 ms, 30.694 ms]
  - `parquet_flush/1000`: 6.0407 s [6.0238 s, 6.0600 s] (1 run)
  - `core_metrics_10k`: 4.1724 ms [4.1462 ms, 4.1992 ms]
  - `timeseries_10k`: 3.0019 ms [2.9860 ms, 3.0181 ms]
  - `breakdown_pages_10k`: 3.5319 ms [3.5102 ms, 3.5541 ms]

- **T3: Property-based tests** — Added proptest property tests to 3 modules:
  - `ingest/visitor_id.rs` (3 tests): determinism, IP uniqueness, daily salt rotation
  - `ingest/ratelimit.rs` (2 tests): bucket independence, monotonic depletion
  - `query/cache.rs` (2 tests): round-trip, disabled-always-misses
  - 7 new tests, all passing.

- **T6: LESSONS.md** — Added L16 (documentation staleness compounds), L17 (security headers need integration tests), L18 (Prometheus counters need end-to-end wiring verification).

**Test results:**
- 219 unit tests passing (`cargo test --lib`)
- 61 integration tests passing (`cargo test --test ingest_test`)
- Total: 280 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)
- Documentation builds without errors (`cargo doc --no-deps`)

### Session 12: Correctness, Performance, and Benchmark Fixes

**Scope:** F1 (event data loss), F2 (blocking async), F3 (Appender API), F4 (benchmark cold-start), F5 (O(n) stat loop), F6 (site_id validation gap). All findings verified from code analysis before implementation.

**Changes:**

- **T1: Fix event data loss on flush failure (F1)** — `flush()` previously called `std::mem::take` to drain the buffer BEFORE DuckDB inserts succeeded. If any insert failed, all drained events were permanently lost. Fix: drain atomically (to prevent double-processing), attempt inserts via Appender (T3), and if Appender creation or any `append_row` fails, restore the drained events to the front of the buffer before returning `Err`. Buffer is cleared only when all inserts succeed. Two new unit tests verify this contract: `test_flush_failure_preserves_events` and `test_flush_partial_failure_restores_all_events`.

- **T2: Fix blocking I/O in tokio::spawn periodic flush (F2)** — The periodic flush task in `spawn_background_tasks()` called `flush_conn.lock()` (blocking mutex) and `storage.flush_events()` (filesystem I/O, 6 s at 1000 events) directly inside a `tokio::spawn` async block. This held an async worker thread, starving the scheduler under load. Fix: `interval.tick().await` remains on the async side; all blocking work is wrapped in `tokio::task::spawn_blocking(move || { ... }).await`. Pattern matches the correct usage in `shutdown_signal()` which already used `spawn_blocking`.

- **T3: Replace row-by-row INSERT with DuckDB Appender API (F3)** — The `for event in &events { conn.execute(...) }` loop executed 1 000 sequential SQL parses and inserts for a 1000-event flush. Replaced with DuckDB's Appender API (`conn.appender("events")` + `appender.append_row(...)` + `appender.flush()`), which uses columnar batch insertion bypassing per-row SQL parsing overhead. Appender lifetime is scoped to an inner block so it drops before subsequent `flush_events()` call on the same connection.

- **T4: Fix benchmark cold-start contamination (F4)** — `bench_buffer_push` and `bench_flush` previously placed `Connection::open_in_memory()` + `schema::init_schema()` + `tempfile::tempdir()` + buffer construction INSIDE `b.iter()`. The ~500 ms DuckDB cold-start dominated every iteration, making timings across all input sizes nearly identical (17 ms for 100 events vs 19 ms for 1000 events). Fix: setup moved OUTSIDE `b.iter()`. `bench_flush` uses `iter_batched` (Criterion's correct pattern for per-iteration state). The old baselines (PERF.md "Superseded Baselines") are explicitly marked as measuring cold-start, not steady-state. `bench_query_metrics` was already correct; unchanged.

- **T5: Fix next_file_path O(n) stat loop (F5)** — `next_file_path()` previously iterated `path.exists()` in a loop, performing one filesystem stat syscall per existing file. After K flushes of the same partition, this was K stat syscalls per new flush. Fixed with a single `fs::read_dir()` call that reads all directory entries, parses the max existing file number, and returns `max + 1`. O(K) stat calls → O(1) directory reads. Two new unit tests verify: `test_next_file_path_with_many_existing_files` (100 existing files → 0101.parquet) and `test_next_file_path_ignores_non_parquet_files` (.tmp and .txt files ignored).

- **T6: Unify site_id validation between ingest and stats (F6)** — The stats API's `validate_site_id()` rejects characters outside `[a-zA-Z0-9._-:]` but the ingest handler only checked length and emptiness. A domain like `"my site.com"` (space) was accepted by `POST /api/event`, stored in Parquet, but permanently unqueryable via any stats endpoint. Fix: `validate_site_id` made `pub(crate)` in `api/stats.rs`; called from the ingest handler after length validation. Events with disallowed characters in domain now return 400. New integration test: `test_ingest_rejects_invalid_site_id_chars`.

- **T8: LESSONS.md** — Added L19 (blocking I/O in tokio::spawn), L20 (std::mem::take before success), L21 (Criterion cold-start contamination).

**Test results (before → after):**
- 219 → 223 unit tests passing (`cargo test --lib`)
- 61 → 62 integration tests passing (`cargo test --test ingest_test`)
- Total: 280 → 285 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)
- Benchmarks compile: `cargo bench --no-run` passes

**New unit tests added (4):**
- `test_flush_failure_preserves_events` — flush with missing table → events preserved in buffer
- `test_flush_partial_failure_restores_all_events` — all 5 events restored after Appender failure
- `test_next_file_path_with_many_existing_files` — 100 existing files → correct next number
- `test_next_file_path_ignores_non_parquet_files` — .tmp and .txt files ignored

**New integration tests added (1):**
- `test_ingest_rejects_invalid_site_id_chars` — domain with space → 400 Bad Request

**Not done (out of scope):**
- T7 (steady-state concurrency benchmarks) — depends on T4 restructure being correct; benchmarks must be run 3× to publish baselines; skipped to stay within session scope
- WAL, compaction, multi-node — explicitly listed as do-not-implement

### Session 13: Production-Readiness Gap Remediation (Continued)

**Scope:** All 20 production-readiness gaps from external audit — CRITICAL (1), HIGH (4), MEDIUM (8), LOW (7).

**Security fixes:**
- **Gap #10 (MEDIUM):** Generate-and-persist `MALLARD_SECRET` — on startup, if `MALLARD_SECRET` env var is absent the server reads `data_dir/.secret`, generates a UUID v4 secret if the file is missing/empty, writes it to disk, and uses it for the session. Prevents visitors being re-fingerprinted after every restart.
- **Gap #8 (MEDIUM):** `/metrics` bearer-token auth — if `MALLARD_METRICS_TOKEN` env var is set, `GET /metrics` requires `Authorization: Bearer <token>` validated with constant-time comparison. Returns 401 Unauthorized otherwise.
- **Gap #9 (MEDIUM):** Cache-Control headers — `Cache-Control: no-store, no-cache` added to all JSON API responses via `add_security_headers` middleware. Prevents proxies/browsers from caching analytics data.
- **Gap #6 (MEDIUM):** `Permissions-Policy` header — `geolocation=(), microphone=(), camera=()` added to all responses.

**Correctness / reliability fixes:**
- **Gap #1 (CRITICAL):** Retention cleanup `spawn_blocking` — `cleanup_old_partitions()` (filesystem I/O) was called directly inside `tokio::spawn`. Wrapped in `tokio::task::spawn_blocking` so it runs on the blocking thread pool, preventing async worker starvation.
- **Gap #14 (LOW):** Hoisted `ParquetStorage::new()` outside the daily retention loop — added `#[derive(Clone)]` to `ParquetStorage` (wraps only a `PathBuf`) and constructs it once before the loop; each iteration clones. Eliminates a heap allocation per daily tick.
- **Gap #11 (MEDIUM):** Concurrent query semaphore — `Arc<tokio::sync::Semaphore>` added to `AppState` (capacity from `MALLARD_MAX_CONCURRENT_QUERIES`, default 10). The four heavy analytics endpoints (`get_retention`, `get_funnel`, `get_sequences`, `get_flow`) acquire a permit before entering `spawn_blocking`; return HTTP 429 `TooManyRequests` if the semaphore is exhausted. `ApiError::TooManyRequests` variant added to `api/errors.rs`.
- **Gap #16 (LOW):** `/health/ready` readiness probe — `GET /health/ready` executes `SELECT 1 FROM events_all LIMIT 0` in a `spawn_blocking`; returns 200 "ready" on success, 503 "database not ready" on failure.
- **Gap #15 (LOW):** `X-Request-ID` header — `add_request_id` middleware injects a `X-Request-ID: <uuid>` header on every response (generates a new UUID v4 if not already set by proxy).
- **Gap #13 (LOW):** Removed `#[allow(dead_code)]` from `EventBuffer::is_empty()` — method is now called in `detailed_health_check` and is genuinely public API.
- **Gap #2 (HIGH):** `CompressionLayer` — `tower_http::compression::CompressionLayer::new()` wired to the router; responses are compressed (gzip/br/zstd) when the client sends an `Accept-Encoding` header.

**Observability fixes:**
- **Gap #9 (MEDIUM):** New Prometheus counters — `mallard_flush_failures_total`, `mallard_rate_limit_rejections_total`, `mallard_login_failures_total`, `mallard_cache_hits_total`, `mallard_cache_misses_total` added to `/metrics`. Counters backed by `AtomicU64` fields on `AppState`; incremented at the relevant sites.
- **Gap #4 (HIGH):** `QueryCache` max-entries cap — `QueryCache::new(ttl_secs, max_entries)` signature extended. When full, expired entries are evicted before insertion; if still full the insert is silently dropped. Default `MALLARD_CACHE_MAX_ENTRIES=10000`. Hit and miss counters (`Arc<AtomicU64>`) added for Prometheus.

**Input validation fixes:**
- **Gap #5 (HIGH):** Date range validation — `StatsParams::date_range()` parses `start_date`/`end_date` as `NaiveDate`, rejects unparseable formats with 400, rejects `end < start` with 400, rejects spans > 366 days with 400.
- **Gap #3 (HIGH):** Breakdown limit cap — `BreakdownParams` now enforces `limit ≤ MAX_BREAKDOWN_LIMIT (1000)`; returns 400 for larger values.
- **Gap #19 (MEDIUM):** Unit-aware `is_safe_interval` — per-unit maximums enforce `seconds ≤ 86400`, `minutes ≤ 1440`, `hours ≤ 720`, `days ≤ 365`, `weeks ≤ 52` to prevent absurdly large interval injections.
- **Gap #17 (LOW):** JSON export `Content-Disposition` — `GET /api/stats/export?format=json` now sends `Content-Disposition: attachment; filename="export.json"` to trigger browser download dialog.

**Build reproducibility fixes:**
- **Gap #18 (LOW):** `--locked` added to all `cargo` invocations in `.github/workflows/ci.yml` (`build`, `test`, MSRV, `bench`) and to both `cargo build` commands in `Dockerfile`.

**Test results (before → after):**
- 223 → 240 unit tests passing (`cargo test --lib`)
- 62 → 62 integration tests passing (`cargo test --test ingest_test`)
- Total: 285 → 302 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)

**New unit tests added (17):**
- `test_cache_max_entries_cap` — insert beyond max_entries is silently dropped
- `test_cache_hits_misses_counters` — hit/miss AtomicU64 counters increment correctly
- `test_metrics_token_auth` — /metrics returns 401 without valid bearer token
- `test_security_headers_present` — OWASP headers present on API response
- `test_cache_control_on_json_api_response` — no-store Cache-Control on JSON endpoints
- `test_readiness_check` — /health/ready returns 200 when DB is reachable

### Session 14: Production-Readiness Gap Remediation (Continued)

**Scope:** 9 patchable gaps and 2 short-term architectural remediations identified by post-Session-13 audit.

**Security / protocol fixes:**
- **HSTS header (MEDIUM):** `Strict-Transport-Security: max-age=31536000; includeSubDomains` added to `add_security_headers()` in `server.rs`. Safe to send unconditionally — browsers only honour HSTS over HTTPS.
- **Ingest 429 `Retry-After` (MEDIUM):** `add_security_headers` middleware injects `Retry-After: 1` on any 429 response that does not already carry the header (the login endpoint sets its own value based on the lockout period). No handler changes required.
- **Cookie `Secure` flag (MEDIUM):** `secure_cookies: bool` added to `Config` (`MALLARD_SECURE_COOKIES` env var, default `false`) and `AppState`. `build_session_cookie` now takes a plain `bool` instead of inferring from `dashboard_origin`. Call sites compute `secure = state.secure_cookies || state.dashboard_origin.as_deref().is_some_and(|o| o.starts_with("https://"))`.
- **`/robots.txt` (MEDIUM):** New route `GET /robots.txt` → `robots_txt()` handler returns `Disallow: /api/`, `/health`, `/metrics`.
- **`/.well-known/security.txt` (LOW):** New route `GET /.well-known/security.txt` → `security_txt()` handler returns RFC 9116 policy with `Contact:` and `Expires:` fields.

**Observability / DX fixes:**
- **Request ID in tracing spans (MEDIUM):** Changed `map_response(add_request_id)` to `from_fn(request_id_middleware)`. New middleware wraps the handler in `tracing::info_span!("http_request", request_id = %id)` and instruments it with `.instrument(span)`, so all log lines emitted during the request carry the same `request_id` field.
- **Silent env var parse failures (LOW):** Added `parse_env_num!` macro in `config.rs` that emits `tracing::warn!` when a numeric env var cannot be parsed, instead of silently falling back to the default.

**Performance / correctness fixes:**
- **Parquet VIEW re-creation skip (LOW):** `flush_events()` in `storage/parquet.rs` guards the `setup_query_view()` call with `if total_flushed > 0`, skipping the expensive glob VIEW re-creation when the flush cycle had no events to write.
- **GET /api/event pixel tracking (LOW):** `GET /api/event` with query-string parameters now returns a 1×1 transparent GIF (43 bytes, `Content-Type: image/gif`). Accepts the same core parameters as POST (`d`, `n`, `u`, `referrer`, `screen_width`). Revenue and props deliberately excluded. Implemented via shared `process_pixel_event()` helper (fire-and-forget). `TRANSPARENT_GIF_1X1` constant moved to module level to satisfy `items_after_statements` clippy lint.

**Architectural remediations:**
- **DuckDB disk-based (SHORT-TERM):** `Connection::open_in_memory()` replaced with `Connection::open(config.db_path())` where `db_path()` returns `data_dir/mallard.duckdb`. Adds `db_path()` method to `Config`. DuckDB WAL ensures atomic batch inserts; hot-buffer events survive SIGKILL (previously lost on crash).
- **ApiKeyStore disk persistence (SHORT-TERM):** `ApiKeyStore` gains `load_from_disk(path: PathBuf)` (loads existing keys from JSON, missing file → empty store) and a private `persist()` (serialise + atomic write). `add_key()` and `revoke_key()` both call `persist()` automatically. `StoredApiKey` gains `#[derive(serde::Serialize, serde::Deserialize)]`. API keys now survive server restarts without needing re-creation.

**Code-quality fixes:**
- `ApiKeyStore::new()` removed (was dead code in binary; callers converted to `ApiKeyStore::default()`). `Default` impl was already present.
- `.map(String::from).unwrap_or_else(...)` → `.map_or_else(...)` in `request_id_middleware` (`clippy::map_unwrap_or`).

**Test results (before → after):**
- 240 → 249 unit tests passing (`cargo test --lib`)
- 62 → 62 integration tests passing (`cargo test --test ingest_test`)
- Total: 302 → 311 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)

**New unit tests added (9):**
- `test_secure_cookies_default_false` — `MALLARD_SECURE_COOKIES` defaults to false
- `test_secure_cookies_flag_overrides_http_origin` — `secure=true` overrides non-HTTPS origin
- `test_db_path` — `db_path()` returns `data_dir/mallard.duckdb`
- `test_warn_on_invalid_env_var_falls_back` — invalid env var → warn + keep default
- `test_api_key_store_persistence_round_trip` — keys written/loaded from disk correctly
- `test_robots_txt` — `/robots.txt` returns correct body
- `test_security_txt` — `/.well-known/security.txt` returns Contact + Expires fields
- `test_pixel_track_returns_gif` — GET /api/event returns 43-byte GIF with `image/gif` content-type
- `test_hsts_header_present` — HSTS header present with correct `max-age`
- `test_retry_after_present_on_query_semaphore_429` — 429 response includes `Retry-After`

### Session 15: Comprehensive Documentation Audit & Observability Wiring

**Scope:** Full documentation audit (all markdown files, all GitHub Pages docs pages, README, SECURITY.md, CONTRIBUTING.md, CLAUDE.md, ROADMAP.md), code wiring for behavioral extension observability, and correctness fixes found during audit.

**Code changes:**
- **`behavioral_extension_loaded` field wired end-to-end** — Added `behavioral_extension_loaded: bool` to `AppState` (in `handler.rs`). Captures the `Ok`/`Err` result of `load_behavioral_extension()` at startup in `main.rs` and propagates it through `build_app_state()`. Exposed in:
  - `GET /health/detailed` JSON response: `"behavioral_extension_loaded": true/false`
  - `GET /metrics` Prometheus gauge: `mallard_behavioral_extension 1/0` (with `# HELP` and `# TYPE` lines)
  - All test fixtures updated (`make_test_state()` in `server.rs`; 6 `AppState` literals in `tests/ingest_test.rs`)

**Documentation fixes (15 issues):**
- **`docs/src/api-reference/index.md`** — Added `GET /api/event` pixel tracking to unauthenticated endpoints list; added `GET /health/ready`; added `GET /robots.txt` and `GET /.well-known/security.txt`; corrected `/metrics` to note optional bearer token; expanded HTTP status codes table (added 404, 413, 503); updated Sections to reflect all endpoints.
- **`docs/src/architecture.md`** — Fixed diagram: `DuckDB (in-memory)` → `DuckDB (disk-based)`; corrected Hot Tier description to reflect disk-based persistence (`data/mallard.duckdb`, WAL durability); corrected restart behavior (DuckDB reopens file rather than starting with empty table).
- **`docs/src/security.md`** — Added `Permissions-Policy`, `Strict-Transport-Security` (HSTS), `Cache-Control`, and `X-Request-ID` to security headers table.
- **`SECURITY.md`** — Fixed `SameSite=Lax` → `SameSite=Strict`; added `MALLARD_SECURE_COOKIES` note; added `Secure` flag inference logic. Replaced single-scope API key description with full two-scope table (`ReadOnly`, `Admin`) and noted disk persistence. Expanded Route Protection table: added `GET /api/event`, `GET /health/ready`, `/robots.txt`, `/.well-known/security.txt`, corrected `/metrics` conditional auth, corrected `/api/keys/*` to list Admin key as valid auth. Added CSRF Protection subsection. Expanded Threat Model table: added CSRF, Clickjacking, Protocol Downgrade mitigations; fixed SameSite value in Session Hijacking row.
- **`docs/src/deployment.md`** — Added `MALLARD_SECURE_COOKIES=true` and `MALLARD_METRICS_TOKEN` to production checklist, Docker run command, Docker Compose environment, and `.env` example. Added `dashboard_origin` checklist item. Added full "Health and Readiness Probes" section with Kubernetes liveness/readiness probe YAML and Docker Compose health check example. Added "After-Proxy Configuration" subsection.
- **`README.md`** — Fixed architecture diagram: `DuckDB (embedded)` → `DuckDB (disk-based)`; added `mallard.duckdb` filename annotation.

**Test results:**
- 249 unit tests passing (`cargo test --lib`)
- 62 integration tests passing (`cargo test --test ingest_test`)
- Total: 311 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)
- Documentation builds without errors (`cargo doc --no-deps`)

### Session 16: Zero-Compromise Enterprise Audit

**Scope:** Full cross-file audit against source of truth (config.rs, server.rs, ci.yml) to find every gap preventing a portfolio-grade, fully auditable repository. Eight prior deploy bugs were fixed in a preceding commit; this session addresses the remaining gaps found during the comprehensive source review.

**Security fixes:**
- **HSTS `preload` directive missing (`src/server.rs`)** — `strict-transport-security` header was `max-age=31536000; includeSubDomains` without the `preload` directive. Added `; preload` to make the header eligible for browser HSTS preload lists (hstspreload.org). Updated `test_hsts_header_present` to also assert `preload` is present.
- **`security.txt` placeholder contact (`src/server.rs`)** — `Contact:` field used `mailto:security@mallard-metrics.example` (a `.example` domain, not a real address) and the comment pointed to `mallard-metrics/mallard-metrics` (wrong repo). Fixed both: `Contact: https://github.com/tomtom215/mallardmetrics/security/advisories/new` (real GitHub private advisory form), correct repo URL in comment, and added a "Do NOT open a public issue" note consistent with SECURITY.md.
- **`SECURITY.md` HSTS row** — Threat model table said "1-year max-age" only. Updated to document all three required directives: `max-age`, `includeSubDomains`, and `preload`.

**CI improvements:**
- **`dtolnay/rust-toolchain@stable` not SHA-pinned** — All six uses of `dtolnay/rust-toolchain` across all CI jobs were using the floating `@stable` tag. Pinned to `efa25f7f19611383d5b0ccf2d1c8914531636bf9` (verified SHA). SECURITY.md claimed "All GitHub Actions pinned to commit SHAs" which was false until this fix.
- **`cargo install cargo-deny --locked` slow and unversioned** — Replaced with `EmbarkStudios/cargo-deny-action@3fd3802e88374d3fe9159b834c7714ec57d6c979` (v2.0.15, verified SHA). Pre-compiled action runs ~60s faster; also removes the unversioned binary install.
- **`cargo install cargo-llvm-cov` slow and unversioned** — Replaced with `taiki-e/cargo-llvm-cov@88655648110d83d256f8c26bd201fd7135564cad` (v0.8.4, verified SHA). Pre-compiled action.
- **Docker job had no image scanning** — Added `aquasecurity/trivy-action@97e0b3872f55f89b95b2f65b3dbab56962816478` (v0.34.2) to the `Docker Build & Scan` job. Scans for CRITICAL and HIGH CVEs in the built image; fails CI on findings. `ignore-unfixed: true` prevents noise from unpatched upstream vulnerabilities.

**Repository community health files (all new):**
- **`.github/ISSUE_TEMPLATE/bug_report.yml`** — Structured GitHub Issue Form for bug reports. Includes pre-flight checklist (no duplicates, not a security vuln, using latest version), deployment method selector, log attachment, and reproduction steps.
- **`.github/ISSUE_TEMPLATE/feature_request.yml`** — Structured form for feature requests. Includes problem statement, proposed solution, alternatives considered, feature area selector, and contribution willingness.
- **`.github/ISSUE_TEMPLATE/config.yml`** — Disables blank issues (`blank_issues_enabled: false`). Contact links route security vulnerabilities to the private advisory form, general questions to Discussions, and documentation to the Pages site.
- **`.github/pull_request_template.md`** — PR checklist matching project standards: all tests pass, zero clippy, zero fmt, docs build, security checklist (SQL injection, path traversal, PII, panics), documentation updated, before/after evidence.
- **`.github/CODEOWNERS`** — Code ownership assignments. Security-sensitive files (`auth.rs`, `visitor_id.rs`, `handler.rs`, `server.rs`, `SECURITY.md`, `deny.toml`), CI/deployment files, and storage layer all require `@tomtom215` review.
- **`CODE_OF_CONDUCT.md`** — Contributor Covenant 2.1 with project-specific enforcement process (private GitHub discussion for reports) and a three-tier enforcement table (minor → warning, repeated → temp ban, severe → permanent ban).

**Test results:**
- 249 unit tests passing (`cargo test --lib`)
- 62 integration tests passing (`cargo test --test ingest_test`)
- Total: 311 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)
- Documentation builds without errors (`cargo doc --no-deps`)

### Session 17: GDPR-Friendly Deployment Mode

**Scope:** Added a first-class GDPR-friendly deployment option with configurable privacy flags, a data erasure API (GDPR Art. 17), and comprehensive documentation.

**Code changes:**

- **`src/config.rs` — 8 new privacy config fields:**
  - `gdpr_mode: bool` — convenience preset that forces a privacy-minimising bundle on startup
  - `strip_referrer_query: bool` — strips `?query` and `#fragment` from stored referrer URLs
  - `round_timestamps: bool` — rounds event timestamps to the nearest hour
  - `suppress_visitor_id: bool` — replaces HMAC visitor hash with a random UUID per request (breaks unique-visitor counting; NOT enabled by `gdpr_mode`)
  - `suppress_browser_version: bool` — stores browser name only, not version
  - `suppress_os_version: bool` — stores OS name only, not version
  - `suppress_screen_size: bool` — omits screen width and device type fields
  - `geoip_precision: String` — precision ladder: `"city"` | `"region"` | `"country"` | `"none"` (default `"city"`)
  - `gdpr_mode` bundle: when `true`, forces all flags above (except `suppress_visitor_id`) and promotes `geoip_precision` from `"city"` → `"country"`
  - New env vars: `MALLARD_GDPR_MODE`, `MALLARD_STRIP_REFERRER_QUERY`, `MALLARD_ROUND_TIMESTAMPS`, `MALLARD_SUPPRESS_VISITOR_ID`, `MALLARD_SUPPRESS_BROWSER_VERSION`, `MALLARD_SUPPRESS_OS_VERSION`, `MALLARD_SUPPRESS_SCREEN_SIZE`, `MALLARD_GEOIP_PRECISION`

- **`src/ingest/handler.rs` — privacy transformations at ingestion time:**
  - Added 8 new fields to `AppState` (all new privacy flags + `events_dir: PathBuf`)
  - Added `pub fn strip_url_query_and_fragment(url: &str) -> &str` helper
  - Added `pub fn round_to_hour(dt: DateTime<Utc>) -> chrono::NaiveDateTime` helper
  - Both `process_pixel_event` and `ingest_event` apply all transforms before any data reaches DuckDB

- **`src/api/stats.rs` — GDPR Art. 17 erasure endpoint:**
  - `DELETE /api/gdpr/erase?site_id=...&start_date=...&end_date=...`
  - Deletes from DuckDB hot table and removes on-disk Parquet partition directories
  - Returns JSON: `status`, `db_records_deleted`, `parquet_partitions_deleted`
  - Requires Admin auth; rejects invalid site_id, bad dates, end < start, spans > 366 days

- **`src/server.rs`** — registered `DELETE /api/gdpr/erase` on admin-auth `key_routes`
- **`src/main.rs`** — wired all 8 new Config fields to AppState; GDPR startup log and retention_days warning

**Documentation:**
- **`PRIVACY.md`** — "GDPR-Friendly Deployment Mode" section with comparison table, activation instructions, all 8 flags, erasure API docs
- **`README.md`** — GDPR feature section, 9 new env vars in Configuration table, erasure endpoint in API Reference
- **`docs/src/deployment.md`** — EU checklist additions, full "GDPR-Friendly Deployment" section
- **`mallard-metrics.toml.example`** — GDPR configuration block with all flags and env var table

**Test results (before → after):**
- 249 → 262 unit tests passing (`cargo test --lib`)
- 62 → 71 integration tests passing (`cargo test --test ingest_test`)
- Total: 311 → 333 tests, 0 failures, 0 ignored
- 0 clippy warnings (`cargo clippy --all-targets`)
- 0 formatting violations (`cargo fmt -- --check`)

**New unit tests added (12):**
- `test_default_gdpr_flags`, `test_gdpr_mode_enables_privacy_bundle`, `test_gdpr_mode_respects_stricter_geoip_precision`, `test_gdpr_mode_respects_region_precision`, `test_validate_invalid_geoip_precision`, `test_validate_valid_geoip_precisions` — config validation
- `test_strip_url_query_and_fragment_query`, `test_strip_url_query_and_fragment_fragment`, `test_strip_url_query_and_fragment_both`, `test_strip_url_query_and_fragment_no_change` — referrer stripping
- `test_round_to_hour_truncates_minutes_seconds`, `test_round_to_hour_on_exact_hour` — timestamp rounding

**New integration tests added (9):**
- `test_gdpr_erase_requires_auth` — 401 without session
- `test_gdpr_erase_returns_ok_for_empty_date_range` — 200 with 0 counts
- `test_gdpr_erase_deletes_hot_events` — range >366 days → 400
- `test_gdpr_erase_deletes_within_366_days` — valid range → 200, events deleted
- `test_gdpr_erase_rejects_invalid_site_id` — site_id with space → 400
- `test_gdpr_erase_rejects_invalid_dates` — non-date string → 400
- `test_gdpr_erase_rejects_end_before_start` — end < start → 400
- `test_gdpr_erase_rejects_range_over_366_days` — >366-day span → 400
- `test_gdpr_erase_accessible_via_admin_api_key` — Admin X-API-Key accepted
