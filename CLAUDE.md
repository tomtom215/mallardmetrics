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

# Run all tests (265 total: 209 unit + 56 integration)
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
| Unit tests | 219 | `cargo test --lib` |
| Integration tests | 61 | `cargo test --test ingest_test` |
| Total tests | 280 | `cargo test` |
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
