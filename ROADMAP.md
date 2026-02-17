# Mallard Metrics — Implementation Roadmap

> Generated 2026-02-17 from a verified audit of the codebase at commit HEAD.
> Every claim below is backed by file:line references verified against actual source code.
> **Nothing is assumed. Nothing is guessed. Nothing is overstated.**

---

## Phase 1: Project Initialization — COMPLETE

**Status:** All 111 tests passing, 0 clippy warnings, 0 format violations.

### What Was Delivered

| Component | Status | Evidence |
|---|---|---|
| Ingestion pipeline (POST /api/event → buffer → Parquet) | Working | `handler.rs:59-147`, `buffer.rs`, 13+6 tests |
| Privacy-safe visitor ID (HMAC-SHA256, daily salt rotation) | Working | `visitor_id.rs`, 8 tests |
| Date-partitioned Parquet storage | Working | `parquet.rs`, 7 tests |
| DuckDB schema + migrations | Working | `schema.rs`, `migrations.rs`, 6 tests |
| Core metrics API (visitors, pageviews, bounce rate) | Working | `metrics.rs`, `stats.rs`, 12 tests |
| Breakdown API (pages, sources, browsers, OS, devices, countries) | Working | `breakdowns.rs`, `stats.rs`, 6+4 tests |
| Timeseries API (daily, hourly) | Working | `timeseries.rs`, 3 tests |
| Tracking script (<1KB, SPA-aware) | Working | `tracking/script.js`, 14 lines |
| Dashboard SPA (Preact + HTM) | Minimal | `dashboard/assets/`, 3 files |
| CI pipeline (11 jobs) | Working | `.github/workflows/ci.yml` |
| Benchmarks (ingest throughput, Parquet flush) | Working | `benches/ingest_bench.rs` |

### Known Gaps Identified Within Phase 1

These are integration gaps where code exists but is not wired up:

| Gap | Detail | Files |
|---|---|---|
| UA parser not called | `parse_user_agent()` exists and is tested (8 tests) but never called from handler — `browser`, `browser_version`, `os`, `os_version` are hardcoded to `None` at `handler.rs:123-126` | `useragent.rs:16`, `handler.rs:123-126` |
| GeoIP not called | `geoip::lookup()` exists (returns None stub) but never called — `country_code`, `region`, `city` are hardcoded to `None` at `handler.rs:129-131` | `geoip.rs:20`, `handler.rs:129-131` |
| Origin validation not enforced | `validate_origin()` exists and is tested (5 tests) but never called from any middleware or handler | `auth.rs:10-22` |
| Dashboard calls 1 of 9 API endpoints | Frontend only calls `/api/stats/main` — the 6 breakdown endpoints and timeseries endpoint exist but have no UI | `app.js:24` |
| Tracking script omits custom props/revenue | Script sends only `d`, `n`, `u`, `r`, `w` — the backend accepts `p` (props), `ra` (revenue_amount), `rc` (revenue_currency) but the script never sends them | `tracking/script.js:5-6`, `handler.rs:39-47` |

---

## Phase 2: Dashboard & Integration Fixes

**Goal:** Wire up the existing backend capabilities to the frontend. Fix the integration gaps from Phase 1. Zero new backend query logic required — everything already has working API endpoints.

### 2.1 — Integrate User-Agent Parser into Ingestion Handler

**What:** Call `parse_user_agent()` from `ingest_event()` so browser/OS fields are populated.

- **Current state:** `handler.rs:123-126` hardcodes `browser: None, browser_version: None, os: None, os_version: None`
- **Required change:** Call `crate::ingest::useragent::parse_user_agent(user_agent)` (the `user_agent` variable already exists at `handler.rs:81-84`) and use the returned `ParsedUserAgent` fields
- **Scope:** ~5 lines changed in `handler.rs`
- **Test plan:** Existing 8 UA parser tests validate the parser. Add 1-2 integration tests verifying browser/OS fields appear in buffered events after ingestion.
- **Risk:** Low. Parser is already tested and the data columns already exist in the schema.

### 2.2 — Integrate GeoIP Stub into Ingestion Handler

**What:** Call `geoip::lookup()` from `ingest_event()` so the GeoIP fields are at least populated (even if they return None for now, the wiring will be ready for Phase 4).

- **Current state:** `handler.rs:129-131` hardcodes `country_code: None, region: None, city: None`
- **Required change:** Call `crate::ingest::geoip::lookup(&ip)` (the `ip` variable already exists at `handler.rs:80`) and use the returned `GeoInfo` fields
- **Scope:** ~4 lines changed in `handler.rs`
- **Test plan:** Existing 2 GeoIP tests validate the stub. Integration tests confirm no regression.
- **Risk:** None. This is a no-op wiring change (stub returns all None).

### 2.3 — Timeseries Chart in Dashboard

**What:** Add a visitor/pageview line chart to the dashboard using the existing `/api/stats/timeseries` endpoint.

- **Current state:** Endpoint is fully functional (`timeseries.rs`, `stats.rs:get_timeseries`), returns `Vec<TimeBucket>` with `date`, `visitors`, `pageviews` fields. Frontend does not call it.
- **Required change:** Add a chart component to `app.js`. Options for chart rendering (no build step required):
  - Canvas-based manual rendering (zero dependencies)
  - `<svg>` elements generated via Preact (zero dependencies)
  - Lightweight library via ESM import (e.g., uPlot from esm.sh)
- **Scope:** New Preact component in `app.js`, CSS additions in `style.css`
- **Test plan:** Manual visual verification + backend already tested (3 tests). Add integration test hitting `/api/stats/timeseries` endpoint.
- **Dependencies:** None. Backend endpoint already registered at `server.rs:20`.

### 2.4 — Breakdown Tables in Dashboard

**What:** Add tables for all 6 breakdown dimensions using existing API endpoints.

- **Current state:** All 6 breakdown endpoints are functional:
  - `/api/stats/breakdown/pages` → `stats.rs:get_pages_breakdown`
  - `/api/stats/breakdown/sources` → `stats.rs:get_sources_breakdown`
  - `/api/stats/breakdown/browsers` → `stats.rs:get_browsers_breakdown`
  - `/api/stats/breakdown/os` → `stats.rs:get_os_breakdown`
  - `/api/stats/breakdown/devices` → `stats.rs:get_devices_breakdown`
  - `/api/stats/breakdown/countries` → `stats.rs:get_countries_breakdown`
- **Each returns:** `Vec<BreakdownRow>` with `value`, `visitors`, `pageviews` fields
- **Required change:** Add tabbed or sectioned breakdown tables to `app.js` that fetch from each endpoint
- **Scope:** New Preact components in `app.js`, CSS additions in `style.css`
- **Test plan:** Backend already tested (6 breakdown tests). Manual visual verification of tables.
- **Dependencies:** Task 2.1 should be completed first so browser/OS breakdowns contain real data.

### 2.5 — Enhanced Tracking Script (Custom Events & Revenue)

**What:** Extend `tracking/script.js` to expose a public API for custom event tracking and revenue attribution.

- **Current state:** Script only sends `d`, `n`, `u`, `r`, `w`. Backend accepts `p` (custom props), `ra` (revenue amount), `rc` (revenue currency) at `handler.rs:39-47`.
- **Required change:** Expose a global function (e.g., `window.mallard('purchase', {props: {...}, revenue: 9.99, currency: 'USD'})`) that sends the additional fields
- **Scope:** ~10-15 lines added to `tracking/script.js`
- **Test plan:** Integration test sending event with all fields and verifying they're stored. Handler already validates field lengths (`handler.rs:70-77`).
- **Risk:** Low. Must maintain <1KB minified size.

### 2.6 — Integrate Origin Validation

**What:** Wire `validate_origin()` into the ingestion handler or as Axum middleware.

- **Current state:** `auth.rs:10-22` — function exists, 5 tests pass, but never called anywhere
- **Required change:** Either call `validate_origin()` from `ingest_event()` or add it as Axum middleware. Needs `allowed_sites` from `Config.site_ids` (`config.rs`).
- **Scope:** ~10-15 lines. Middleware approach is cleaner.
- **Test plan:** Existing 5 tests cover the function. Add integration tests for rejected origins.
- **Risk:** Low. Must not break existing ingestion if no sites configured (already handled — returns `true` when empty).

### Phase 2 Exit Criteria

- [ ] Browser/OS fields populated in events after ingestion (verify with query)
- [ ] GeoIP lookup wired (even if returning None)
- [ ] Dashboard displays timeseries chart
- [ ] Dashboard displays all 6 breakdown tables
- [ ] Tracking script supports custom events and revenue
- [ ] Origin validation enforced on /api/event
- [ ] All existing 111 tests still pass + new tests added
- [ ] 0 clippy warnings, 0 format violations

---

## Phase 3: Behavioral Analytics (Advanced Queries)

**Goal:** Promote the existing SQL builders from string-returning stubs to fully executable, tested, API-exposed query functions with dashboard UI.

**Prerequisite:** The `behavioral` DuckDB extension must be loadable at runtime. Currently loaded at `schema.rs:load_behavioral_extension`. These queries will fail gracefully if the extension is unavailable (following the pattern at `metrics.rs:query_bounce_rate`).

### 3.1 — Session Analytics API + UI

**What:** Create API endpoints for session metrics and add a dashboard section.

- **Current state:** `sessions.rs:15-57` — `query_session_metrics()` is fully implemented, executes real SQL with `sessionize()`, returns `SessionMetrics`. Marked `#[allow(dead_code)]`. No API endpoint. No UI.
- **Required changes:**
  - Add `GET /api/stats/sessions` endpoint in `stats.rs` that calls `query_session_metrics()`
  - Register route in `server.rs`
  - Add session metrics cards to dashboard (total sessions, avg duration, avg pages/session)
- **Scope:** ~30 lines backend (new handler + route), ~20 lines frontend
- **Test plan:** `sessions.rs` has 1 conditional test. Add integration test with behavioral extension loaded.
- **Risk:** Medium. Requires behavioral extension. Must handle gracefully when unavailable.

### 3.2 — Funnel Analysis API + UI

**What:** Replace the 501 stub with a real funnel endpoint and add a funnel visualization.

- **Current state:**
  - `funnel.rs:14-57` — `query_funnel()` is fully implemented with `window_funnel()`, executes real SQL, returns `Vec<FunnelStep>`. Marked `#[allow(dead_code)]`.
  - `funnels.rs:6-8` — API handler returns `StatusCode::NOT_IMPLEMENTED` (501)
  - Route already registered at `server.rs:39`
- **Required changes:**
  - Replace `funnels.rs` stub with real handler that accepts funnel step definitions and calls `query_funnel()`
  - Design funnel step input format (query params or POST body for step conditions)
  - Add funnel visualization to dashboard (horizontal bar chart showing drop-off)
- **Scope:** ~50 lines backend (handler + input parsing), ~40 lines frontend
- **Test plan:** `funnel.rs` has 1 test (empty steps). Add tests with real funnel data.
- **Risk:** Medium. Funnel step conditions are SQL expressions — must prevent SQL injection. Currently the docstring at `funnel.rs:29` notes step conditions are "defined by the application, not user input." If exposing via API, conditions must be restricted to safe patterns (e.g., predefined condition templates like `event_name = ?`, `pathname = ?`).

### 3.3 — Retention Cohort API + UI

**What:** Make `query_retention()` execute the SQL and return results, add API endpoint and dashboard view.

- **Current state:** `retention.rs:14-51` — builds SQL string but suppresses all parameters with `let _ = conn;` and returns the raw SQL string. Does not execute.
- **Required changes:**
  - Refactor `query_retention()` to execute the query and parse results into `Vec<RetentionCohort>`
  - Add `GET /api/stats/retention` endpoint
  - Register route in `server.rs`
  - Add cohort grid/table to dashboard
- **Scope:** ~40 lines backend refactor, ~30 lines new handler, ~50 lines frontend (cohort grid)
- **Test plan:** Current test (`retention.rs:57-64`) only validates SQL generation. Replace with execution test.
- **Risk:** Medium. The `retention()` function from behavioral extension returns a `BOOLEAN[]` array. DuckDB Rust bindings may need special handling for array types. Must verify against actual DuckDB output (LESSONS.md L5).

### 3.4 — Sequence Analysis API + UI

**What:** Make the sequence SQL builders (`sequences.rs`) execute and return results.

- **Current state:**
  - `sequences.rs:14-28` — `build_sequence_match_sql()` returns SQL string only
  - `sequences.rs:33-43` — `build_sequence_count_sql()` returns SQL string only
  - Both return `String`, neither executes
- **Required changes:**
  - Create `execute_sequence_match()` and `execute_sequence_count()` functions that prepare + execute the SQL
  - Create response structs for results (note: `SequenceMatchResult` already exists at `sequences.rs:2-7`)
  - Add `GET /api/stats/sequences` endpoint
  - Register route in `server.rs`
  - Add sequence analysis view to dashboard
- **Scope:** ~40 lines backend execution, ~30 lines handler, ~30 lines frontend
- **Test plan:** Current tests (`sequences.rs:50-68`) only validate SQL shape. Replace with execution tests.
- **Risk:** Medium-High. Sequence patterns contain arbitrary SQL-like syntax. Must restrict allowed patterns to prevent injection. The `pattern` parameter is interpolated directly into the SQL string at `sequences.rs:17,36`.

### 3.5 — Flow Analysis API + UI

**What:** Make `build_flow_sql()` execute and return results.

- **Current state:** `flow.rs:5-20` — builds SQL string with `sequence_next_node()`, returns `String`. Does not execute. The `target_page` parameter is interpolated directly into the SQL string at `flow.rs:11`.
- **Required changes:**
  - Create `query_flow()` function that prepares + executes the SQL
  - Add `GET /api/stats/flow` endpoint with `page` query parameter
  - Register route in `server.rs`
  - Add flow visualization to dashboard (Sankey diagram or next-page table)
- **Scope:** ~30 lines backend, ~20 lines handler, ~40 lines frontend
- **Test plan:** Current test (`flow.rs:27-32`) only validates SQL shape. Replace with execution test.
- **Risk:** High. `target_page` at `flow.rs:11` is interpolated directly into SQL via `format!()`. **This is a SQL injection vector.** Must be refactored to use parameterized queries before exposing via API. This conflicts with DuckDB's `sequence_next_node()` syntax which may not support parameters in condition positions — needs investigation.

### Phase 3 Exit Criteria

- [ ] Session metrics API endpoint functional with behavioral extension
- [ ] Funnel API replaces 501 stub, with safe condition handling
- [ ] Retention API executes queries and returns cohort data
- [ ] Sequence match/count APIs functional with injection-safe patterns
- [ ] Flow analysis API functional with parameterized queries (no SQL injection)
- [ ] All new features degrade gracefully without behavioral extension
- [ ] Dashboard displays all 5 advanced analytics views
- [ ] All existing tests pass + new tests for each feature
- [ ] 0 clippy warnings, 0 format violations

---

## Phase 4: Production Hardening

**Goal:** Make the system production-ready with real GeoIP, authentication, robust UA parsing, and security improvements.

### 4.1 — MaxMind GeoLite2 Integration

**What:** Replace the GeoIP stub with real IP-to-location resolution.

- **Current state:** `geoip.rs:20-22` returns `GeoInfo::default()` (all None). Documented for Phase 4.
- **Required changes:**
  - Add `maxminddb` crate dependency
  - Implement `lookup()` using MaxMind GeoLite2 `.mmdb` file
  - Add configuration for GeoLite2 database path in `config.rs`
  - Handle missing/expired database file gracefully
  - Country code, region, and city resolution
- **Scope:** ~60-80 lines in `geoip.rs`, ~10 lines in `config.rs`
- **New dependency:** `maxminddb` crate
- **External requirement:** MaxMind GeoLite2 database file (free with registration, requires license key for download)
- **Test plan:** Replace existing 2 stub tests with real lookup tests. Mock or use a test `.mmdb` file.
- **Risk:** Medium. MaxMind database is ~60MB (City) or ~5MB (Country). Must decide which to bundle or require external download. Must handle IP privacy correctly — lookup happens in-memory, IP is never stored (existing design already ensures this at `visitor_id.rs`).
- **Privacy note:** IP addresses are only used for hashing (visitor ID) and GeoIP lookup. They are never written to DuckDB or Parquet. This must remain true.

### 4.2 — Dashboard Authentication

**What:** Add username/password authentication to protect the dashboard.

- **Current state:** `auth.rs:1-6` documents Phase 4 authentication plan — "username/password (bcrypt/argon2) and API key management." Dashboard is currently open.
- **Required changes:**
  - Add password hashing dependency (`argon2` or `bcrypt` crate)
  - Add user credentials storage (likely a `users` table in DuckDB or a separate config file)
  - Add login page to dashboard frontend
  - Add session cookie or JWT token management
  - Add Axum middleware/extractor for route protection
  - Protect all `/api/stats/*` and dashboard routes
  - Keep `/api/event` and `/health` unauthenticated (ingestion must work without auth)
- **Scope:** ~150-200 lines backend (auth middleware, login handler, user storage), ~60 lines frontend (login form)
- **New dependencies:** `argon2` or `bcrypt`, possibly `jsonwebtoken` for JWTs or `cookie` for session cookies
- **Test plan:** Test login flow, token validation, protected route rejection, unauthenticated ingestion.
- **Risk:** Medium-High. Authentication is security-critical. Must handle:
  - Timing-safe password comparison
  - Secure cookie attributes (HttpOnly, Secure, SameSite)
  - Brute-force protection (rate limiting)
  - Session expiration

### 4.3 — Full User-Agent Parser

**What:** Replace the simple string-matching UA parser with a comprehensive parser library.

- **Current state:** `useragent.rs:1-2` — "Phase 1 uses simple string matching. Phase 4 will integrate a full UA parser." Current parser handles Chrome, Firefox, Safari, Edge, Opera + 6 OS families with basic version extraction. It misses: bot detection, less common browsers (Samsung Internet, UC Browser, Brave, Vivaldi), and can misclassify unusual UA strings.
- **Options:**
  - `woothee` crate — Rust-native, well-maintained, comprehensive
  - `uaparser` crate — Based on ua-parser regexes
  - Keep current approach and extend manually
- **Scope:** ~20 lines if using a library (replace detect functions), ~100+ lines if extending manually
- **Test plan:** Keep existing 8 tests as regression suite, add tests for edge cases (bots, rare browsers).
- **Risk:** Low. Drop-in replacement for existing interface.

### 4.4 — API Key Management

**What:** Allow programmatic API access with API keys for non-browser clients.

- **Current state:** No API key infrastructure exists.
- **Required changes:**
  - API key generation and storage
  - API key validation middleware
  - Key scoping (read-only stats vs. full access)
  - Key rotation support
- **Scope:** ~100 lines backend
- **Dependencies:** Task 4.2 (authentication) should be completed first as a foundation.
- **Risk:** Medium. Key storage must be secure. Keys should be hashed at rest.

### 4.5 — CORS Hardening

**What:** Replace the permissive `Any` CORS policy with site-specific origins.

- **Current state:** `server.rs:13-15` — `CorsLayer::new().allow_origin(Any).allow_methods(Any).allow_headers(Any)`. This is intentionally permissive for Phase 1 (tracking script needs to POST from any origin).
- **Required changes:**
  - `/api/event` must remain permissive (tracking script POSTs from customer domains)
  - `/api/stats/*` and dashboard routes should restrict origins to the dashboard host
  - Split CORS configuration per route group
- **Scope:** ~20 lines in `server.rs`
- **Test plan:** Extend CORS integration test at `server.rs:224-245`.
- **Dependencies:** Task 4.2 (authentication) provides the complementary security layer.
- **Risk:** Low if done carefully. Must not break tracking script cross-origin POSTs.

### Phase 4 Exit Criteria

- [ ] GeoIP resolves country (at minimum) for valid IPs
- [ ] Dashboard protected by authentication
- [ ] Login/logout flow works end-to-end
- [ ] Full UA parser covers major browsers, OSes, and bots
- [ ] API key CRUD and validation functional
- [ ] CORS tightened for dashboard routes
- [ ] Ingestion still works without authentication
- [ ] All tests pass + new tests for each feature
- [ ] 0 clippy warnings, 0 format violations
- [ ] No PII stored (IP addresses only used for hashing and GeoIP, then discarded)

---

## Phase 5: Operational Excellence

**Goal:** Make the system reliable, maintainable, and production-grade for long-running deployments.

### 5.1 — Data Retention & Cleanup

**What:** Automatic deletion of old Parquet files based on a configurable retention period.

- **Current state:** No retention policy. Parquet files accumulate indefinitely in `data/events/site_id=*/date=*/`.
- **Required changes:**
  - Add `retention_days` config option to `config.rs`
  - Background task to scan and delete Parquet partitions older than retention period
  - Log deleted partitions for auditability
- **Scope:** ~50 lines (background task + config)
- **Risk:** Medium. Deletion is irreversible. Must be well-tested and logged.

### 5.2 — Data Export & Backup

**What:** API endpoint or CLI command to export/backup analytics data.

- **Current state:** Raw Parquet files are the only data format. No export mechanism.
- **Required changes:**
  - `GET /api/export` endpoint that returns a CSV or Parquet download for a site/date range
  - Or CLI subcommand (`mallard-metrics export --site example.com --from 2024-01-01 --to 2024-12-31`)
- **Scope:** ~80-100 lines
- **Dependencies:** Task 4.2 (authentication) — export should be authenticated.
- **Risk:** Low. DuckDB's `COPY TO` supports CSV/Parquet natively.

### 5.3 — Graceful Shutdown & Signal Handling

**What:** Flush the in-memory event buffer on shutdown to prevent data loss.

- **Current state:** `main.rs` starts the server but has no shutdown handler. If the process is killed, any events in the buffer are lost. The buffer can hold up to `flush_event_count` events (default 1000, configured at `config.rs`).
- **Required changes:**
  - Handle SIGTERM/SIGINT signals
  - Flush the event buffer before exiting
  - Add graceful shutdown timeout
- **Scope:** ~20-30 lines in `main.rs`
- **Risk:** Low. Tokio provides `signal::ctrl_c()` and Axum supports `with_graceful_shutdown()`.

### 5.4 — Health Check Enhancement

**What:** Make the health check endpoint report meaningful system status.

- **Current state:** `server.rs:52-54` — returns the string "ok" unconditionally.
- **Required changes:**
  - Check DuckDB connection health
  - Report buffer size and last flush time
  - Report disk usage of data directory
  - Return structured JSON instead of plain text
- **Scope:** ~30 lines
- **Risk:** None.

### 5.5 — Structured Logging Improvements

**What:** Add operational metrics and request logging.

- **Current state:** `tracing` and `tracing-subscriber` are dependencies. `TraceLayer` is applied at `server.rs:47`. Only one explicit `tracing::error!()` call exists at `handler.rs:143`.
- **Required changes:**
  - Add structured fields to ingestion logs (site_id, event_name, buffer_size)
  - Log flush operations (events flushed, Parquet files written, duration)
  - Log startup configuration
  - Request duration logging (already partially handled by TraceLayer)
- **Scope:** ~30-40 lines across modules
- **Risk:** None.

### 5.6 — Configuration File Template

**What:** Provide a documented example configuration file.

- **Current state:** No example config file exists. Configuration is documented only in `config.rs` source code.
- **Required changes:**
  - Create `mallard.example.toml` with all options documented
  - Create `.env.example` with all environment variables
- **Scope:** 2 small files
- **Risk:** None.

### 5.7 — Docker Optimization

**What:** Add `.dockerignore` and optimize the Docker build.

- **Current state:** No `.dockerignore` file exists. Docker builds may copy unnecessary files (target/, .git/, data/).
- **Required changes:**
  - Create `.dockerignore` (exclude target, .git, data, *.parquet)
  - Consider multi-platform builds (ARM64 for Apple Silicon / AWS Graviton)
- **Scope:** 1 small file + optional CI changes
- **Risk:** None.

### Phase 5 Exit Criteria

- [ ] Old Parquet files cleaned up automatically based on retention policy
- [ ] Data can be exported for backup purposes
- [ ] Buffer is flushed on graceful shutdown (no data loss on SIGTERM)
- [ ] Health check reports system status as JSON
- [ ] Key operations are logged with structured fields
- [ ] Example config files provided
- [ ] Docker build optimized with .dockerignore
- [ ] All tests pass + new tests for retention and export
- [ ] 0 clippy warnings, 0 format violations

---

## Phase 6: Scale & Performance (Future)

**Goal:** Optimize for higher traffic and larger datasets. Only pursue if real-world usage demands it.

### Potential Tasks (Not Yet Scoped)

These are identified as potential needs but are NOT planned in detail because they depend on actual production usage data. Scoping them now would be premature speculation.

- **Write-ahead log (WAL):** If buffer data loss on crash becomes a real concern (Phase 5.3 may suffice)
- **Parquet compaction:** Merge many small Parquet files per partition into fewer large ones
- **Query caching:** Cache expensive query results with TTL
- **Connection pooling:** If concurrent query load requires it (currently single connection behind Mutex)
- **Rate limiting:** On the ingestion endpoint if abuse becomes a concern
- **Multi-node:** Only if single-process can't handle the load (likely far off — DuckDB is very fast)

---

## Summary: Implementation Order

| Phase | Key Deliverable | Estimated Scope | Dependencies |
|---|---|---|---|
| **Phase 2** | Full dashboard + integration fixes | 6 tasks | None (all backend exists) |
| **Phase 3** | Behavioral analytics (funnel, retention, flow) | 5 tasks | Behavioral extension runtime |
| **Phase 4** | Auth, GeoIP, UA parser, API keys | 5 tasks | MaxMind database file |
| **Phase 5** | Ops (retention, backup, shutdown, logging) | 7 tasks | Phase 4 for export auth |
| **Phase 6** | Performance (if needed) | TBD | Production usage data |

### Critical Path

```
Phase 2.1 (UA integration) → Phase 2.4 (breakdowns have real data)
Phase 2.2 (GeoIP wiring)   → Phase 4.1 (MaxMind drops in)
Phase 2.6 (origin validation) → Phase 4.2 (full auth builds on this)
Phase 4.2 (authentication) → Phase 4.4 (API keys), Phase 5.2 (export auth)
```

### SQL Injection Risks Requiring Attention

These must be resolved before exposing via API:

| File | Line | Issue |
|---|---|---|
| `flow.rs:11` | `pathname = '{target_page}'` | `target_page` interpolated directly into SQL |
| `sequences.rs:17,36` | `'{pattern}'` and `{conds}` | Pattern and conditions interpolated directly |
| `funnel.rs:35-36` | `INTERVAL '{window_interval}'` and `{step_conditions}` | Window interval and conditions interpolated directly |
| `retention.rs:25` | `INTERVAL '{i} weeks'` | Safe (integer only) but `retention_args` built from format strings |

**Note:** These are currently safe because the functions are `#[allow(dead_code)]` and never called from API handlers. They become injection vectors the moment they're exposed via HTTP endpoints. Phase 3 must address this before wiring them up.

---

## Verification Protocol

For every phase, the session must:

1. Run `cargo test` — all tests pass
2. Run `cargo clippy --all-targets` — 0 warnings
3. Run `cargo fmt -- --check` — 0 violations
4. Run `cargo doc --no-deps` — builds clean
5. Verify every new claim with evidence
6. Update CLAUDE.md session log
7. Update test counts in CLAUDE.md metrics table
