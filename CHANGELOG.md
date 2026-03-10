# Changelog

All notable changes to Mallard Metrics will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## Table of Contents

- [Unreleased](#unreleased)

---

## [Unreleased]

### Added

#### Phase 1: Project Initialization

- Rust project with Axum 0.8, DuckDB 1.4.4 (embedded), Rust 1.93.0 MSRV
- Event ingestion endpoint (`POST /api/event`) with in-memory buffer and periodic flush
- Privacy-safe visitor ID generation (HMAC-SHA256 with daily salt rotation)
- Date-partitioned Parquet storage with ZSTD compression (`site_id` + `date` partitioning)
- DuckDB 25-column events table schema with migration system
- Core metrics queries: unique visitors, total pageviews, bounce rate (sessionize)
- Dimension breakdowns: pages, referrer sources, countries, browsers, OS, devices
- Time-series aggregation with hourly and daily granularity
- Behavioral analytics query builders: funnel (`window_funnel`), retention, sessions (`sessionize`), sequences (`sequence_match`/`sequence_count`), flow (`sequence_next_node`)
- Dashboard SPA (Preact + HTM, embedded in binary via `rust-embed`)
- Tracking script (under 1KB minified JavaScript)
- Health check endpoint (`GET /health`)
- CORS support via `tower-http`
- User-Agent parsing (browser, OS, version detection)
- Referrer source detection (Google, Bing, Twitter, Facebook, Reddit, etc.)
- UTM parameter extraction
- Input validation and sanitization
- CI pipeline (build, test, clippy, fmt, docs, MSRV, bench, security, coverage, docker)
- Criterion.rs benchmark suite for ingestion throughput and Parquet flush
- Dockerfile (multi-stage, `FROM scratch`)
- `docker-compose.yml` with persistent storage

#### Phase 2: Dashboard and Integration Fixes

- Integrated User-Agent parser into ingestion handler (populates browser, OS, version fields)
- Integrated GeoIP stub into ingestion handler (wired for Phase 4 MaxMind integration)
- Time-series line chart in dashboard (SVG-based, visitors and pageviews)
- All 6 breakdown tables in dashboard (pages, sources, browsers, OS, devices, countries)
- Enhanced tracking script with custom event API (`window.mallard()`) and revenue tracking
- Origin validation enforced on ingestion endpoint

#### Phase 3: Behavioral Analytics

- `GET /api/stats/sessions` endpoint with session metrics (total sessions, avg duration, pages/session)
- `GET /api/stats/funnel` endpoint with safe `page:/path` and `event:name` step format
- `GET /api/stats/retention` endpoint with cohort grid data and `BOOLEAN[]` parsing
- `GET /api/stats/sequences` endpoint with safe pattern generation from conditions
- `GET /api/stats/flow` endpoint with SQL injection prevention for target page
- Dashboard views for all 5 advanced analytics features (sessions, funnel, retention, sequences, flow)
- Graceful degradation for all behavioral queries when the extension is unavailable

#### Phase 4: Production Hardening

- Argon2id password hashing for dashboard authentication
- Session management with 256-bit cryptographic tokens (HttpOnly cookies)
- API key management (create, list, revoke) with SHA-256 hashed storage and `mm_` prefix
- Auth middleware protecting stats, key management, and export routes
- CORS hardening: permissive for ingestion, restrictive for dashboard
- MaxMind GeoLite2 GeoIP reader with graceful fallback
- Bot traffic filtering via User-Agent detection

#### Phase 5: Operational Excellence

- Data retention cleanup (`cleanup_old_partitions()`) with configurable `MALLARD_RETENTION_DAYS`
- Data export API (`GET /api/stats/export`) with CSV and JSON format support
- Graceful shutdown with SIGINT/SIGTERM handling and buffered event flush
- Enhanced health check (`GET /health/detailed`) with JSON system status
- Structured logging with `MALLARD_LOG_FORMAT=json` option
- Configuration template (`mallard-metrics.toml.example`) with all options documented
- Docker build optimization with dependency caching layer

#### Phase 6: Scale and Performance

- TTL-based in-memory query result cache (`query/cache.rs`) for stats and timeseries endpoints
- Per-site token-bucket rate limiter (`ingest/ratelimit.rs`) for ingestion endpoint
- Query benchmarks (core metrics, timeseries, breakdowns) added to Criterion suite
- Prometheus metrics endpoint (`GET /metrics`) with `text/plain; version=0.0.4` format

#### Phase 7: Security and Production Readiness

- Brute-force protection: `LoginAttemptTracker` with per-IP lockout; returns 429 after configurable failures; `MALLARD_MAX_LOGIN_ATTEMPTS` and `MALLARD_LOGIN_LOCKOUT` env vars
- Body size limit: `DefaultBodyLimit::max(65_536)` on ingestion routes; returns 413 on overflow
- OWASP security headers middleware: `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: strict-origin-when-cross-origin`, `Content-Security-Policy` (HTML responses only)
- HTTP timeout: `TimeoutLayer` with 30-second limit prevents Slowloris-style attacks
- CSRF protection: `validate_csrf_origin()` enforced on all session-auth state-mutating routes
- API key scope enforcement: `require_admin_auth` middleware returns 403 for `ReadOnly` keys on key management routes
- `X-API-Key` header supported as alternative to `Authorization: Bearer` for API key auth
- IP audit logging for all auth events (login failures, lockouts, setup, logout, key operations); IPs anonymized before logging
- Prometheus counter `mallard_events_ingested_total` (`AtomicU64`) wired end-to-end through ingest handler
- Config validation at startup: `Config::validate()` exits with error code 1 on invalid settings
- `site_id` validation on all stats endpoints: rejects empty, >256 chars, or non-ASCII-alphanumeric values
- Revoked API key garbage collection runs in a 15-minute background task
- Dashboard export download buttons for CSV and JSON formats
- Funnel chart division-by-zero guard in dashboard JavaScript
- Local JS bundles (`preact.js` + `htm.js`) served via `rust-embed`; CDN dependency eliminated

#### Correctness and Reliability

- Fixed event data loss on flush failure: drained events are restored to the buffer if DuckDB insertion fails
- Fixed blocking I/O in `tokio::spawn` periodic flush: wrapped in `spawn_blocking` to avoid async worker starvation
- Replaced row-by-row `INSERT` with DuckDB Appender API for batch columnar insertion
- Fixed `next_file_path` O(n) stat loop: replaced with single `read_dir` call
- Unified `site_id` validation between ingest and stats endpoints
- Fixed Parquet query gap: `events_all` VIEW unions hot DuckDB table with cold Parquet files
- Fixed `shutdown_timeout_secs` enforcement: flush wrapped in `tokio::time::timeout`
- Fixed `validate_origin` prefix-bypass vulnerability (`example.com.evil.com` no longer matches `example.com`)
- DuckDB disk-based storage: `Connection::open(data_dir/mallard.duckdb)` replaces in-memory; WAL ensures crash durability
- API key store disk persistence: keys survive server restarts via JSON serialization

#### Production Infrastructure

- HSTS header with `max-age`, `includeSubDomains`, and `preload` directives
- `Retry-After` header on all 429 responses
- Cookie `Secure` flag configurable via `MALLARD_SECURE_COOKIES`
- `GET /robots.txt` and `GET /.well-known/security.txt` endpoints
- `X-Request-ID` header with tracing span integration for log correlation
- Concurrent query semaphore (`MALLARD_MAX_CONCURRENT_QUERIES`, default 10)
- `GET /health/ready` readiness probe (queries DuckDB; returns 503 if not ready)
- `CompressionLayer` for gzip/br/zstd response compression
- `Cache-Control: no-store, no-cache` on all JSON API responses
- `Permissions-Policy` header (geolocation, microphone, camera disabled)
- `GET /api/event` pixel tracking (1x1 transparent GIF)
- Auto-generated `MALLARD_SECRET` persisted to `data_dir/.secret`
- `/metrics` optional bearer-token auth via `MALLARD_METRICS_TOKEN`
- Query cache max-entries cap (`MALLARD_CACHE_MAX_ENTRIES`, default 10000)
- Date range validation (max 366 days, end >= start)
- Breakdown limit cap (max 1000)
- `--locked` flag on all CI `cargo` invocations
- Trivy container image scanning in CI
- `Strict-Transport-Security` preload directive
- `security.txt` with real GitHub advisory contact URL
- SHA-pinned `dtolnay/rust-toolchain` in CI
- `cargo-deny-action` and `cargo-llvm-cov` pre-compiled CI actions

#### GDPR-Friendly Deployment

- `MALLARD_GDPR_MODE` convenience preset for privacy-minimising configuration
- `strip_referrer_query`: strip `?query` and `#fragment` from stored referrers
- `round_timestamps`: round event timestamps to nearest hour
- `suppress_visitor_id`: replace HMAC hash with random UUID per request
- `suppress_browser_version` / `suppress_os_version`: store name only
- `suppress_screen_size`: omit screen width and device type
- `geoip_precision`: configurable ladder (`city`, `region`, `country`, `none`)
- `DELETE /api/gdpr/erase` endpoint for GDPR Art. 17 right-to-erasure requests

#### Documentation

- GitHub Pages documentation site (mdBook) with 13 pages
- `deploy-flyio.md`: complete Fly.io deployment guide
- `deploy-vps.md`: complete VPS deployment guide with LUKS, Caddy, and vps-audit
- `PRIVACY.md`: GDPR/ePrivacy/CCPA analysis with legal citations
- `PERF.md`: benchmark framework and baselines
- `LESSONS.md`: 21 development lessons learned
- `SECURITY.md`: security model, threat model, and vulnerability reporting
- GitHub issue templates (bug report, feature request)
- Pull request template with security checklist
- `CODEOWNERS` file
- `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1)

#### Property-Based and Benchmark Testing

- 7 proptest property tests (visitor_id, ratelimit, cache)
- Criterion benchmark suite restructured: setup moved outside `b.iter()`
- Prometheus counters for flush failures, rate limit rejections, login failures, cache hits/misses
