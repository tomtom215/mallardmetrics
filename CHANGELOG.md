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
- CI pipeline with 10 jobs (build, test, clippy, fmt, docs, MSRV, bench, security, coverage, docker)
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
