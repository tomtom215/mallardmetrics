# Roadmap

This document tracks the implementation phases of Mallard Metrics, including completed work and future plans.

---

## Table of Contents

- [Status Overview](#status-overview)
- [Phase 1: Project Initialization -- COMPLETE](#phase-1-project-initialization----complete)
- [Phase 2: Dashboard and Integration Fixes -- COMPLETE](#phase-2-dashboard-and-integration-fixes----complete)
- [Phase 3: Behavioral Analytics -- COMPLETE](#phase-3-behavioral-analytics----complete)
- [Phase 4: Production Hardening -- COMPLETE](#phase-4-production-hardening----complete)
- [Phase 5: Operational Excellence -- COMPLETE](#phase-5-operational-excellence----complete)
- [Phase 6: Scale and Performance -- COMPLETE](#phase-6-scale-and-performance----complete)
- [Future Considerations](#future-considerations)
- [Implementation Summary](#implementation-summary)

---

## Status Overview

| Phase | Status | Key Deliverable |
|---|---|---|
| Phase 1 | COMPLETE | Core ingestion pipeline, storage, queries, dashboard, CI |
| Phase 2 | COMPLETE | Full dashboard integration, tracking script enhancements |
| Phase 3 | COMPLETE | Behavioral analytics (funnels, retention, sessions, sequences, flow) |
| Phase 4 | COMPLETE | Auth, GeoIP, API keys, CORS hardening, bot filtering |
| Phase 5 | COMPLETE | Data retention, export, graceful shutdown, logging, health checks |
| Phase 6 | COMPLETE | Query caching, rate limiting, benchmarks, Prometheus metrics |

**Current test suite:** 226 tests (183 unit + 43 integration), 0 clippy warnings, 0 format violations.

---

## Phase 1: Project Initialization -- COMPLETE

### Delivered

| Component | Evidence |
|---|---|
| Ingestion pipeline (`POST /api/event` -> buffer -> Parquet) | `handler.rs`, `buffer.rs`, `parquet.rs` |
| Privacy-safe visitor ID (HMAC-SHA256, daily salt rotation) | `visitor_id.rs` |
| Date-partitioned Parquet storage | `parquet.rs` |
| DuckDB schema + migrations | `schema.rs`, `migrations.rs` |
| Core metrics API (visitors, pageviews, bounce rate) | `metrics.rs`, `stats.rs` |
| Breakdown API (pages, sources, browsers, OS, devices, countries) | `breakdowns.rs`, `stats.rs` |
| Timeseries API (daily, hourly) | `timeseries.rs` |
| Tracking script (under 1KB) | `tracking/script.js` |
| Dashboard SPA (Preact + HTM) | `dashboard/assets/` |
| CI pipeline (10 jobs) | `.github/workflows/ci.yml` |
| Criterion.rs benchmarks | `benches/ingest_bench.rs` |

---

## Phase 2: Dashboard and Integration Fixes -- COMPLETE

### Delivered

| Task | Description |
|---|---|
| 2.1 | Integrated User-Agent parser into ingestion handler -- browser, OS, version fields populated |
| 2.2 | Integrated GeoIP stub into ingestion handler -- wired for MaxMind integration |
| 2.3 | Added time-series SVG line chart to dashboard (visitors + pageviews) |
| 2.4 | Added all 6 breakdown tables to dashboard with responsive grid layout |
| 2.5 | Enhanced tracking script with `window.mallard()` custom event API and revenue tracking |
| 2.6 | Integrated origin validation into ingestion handler |

---

## Phase 3: Behavioral Analytics -- COMPLETE

All advanced analytics features use the DuckDB `behavioral` extension and degrade gracefully when it is unavailable.

### Delivered

| Task | Endpoint | Description |
|---|---|---|
| 3.1 | `GET /api/stats/sessions` | Session metrics (total sessions, avg duration, pages/session) via `sessionize()` |
| 3.2 | `GET /api/stats/funnel` | Funnel analysis with safe `page:/path` and `event:name` step format via `window_funnel()` |
| 3.3 | `GET /api/stats/retention` | Retention cohort grid with `BOOLEAN[]` parsing via `retention()` |
| 3.4 | `GET /api/stats/sequences` | Sequence pattern matching with safe condition generation via `sequence_match()` |
| 3.5 | `GET /api/stats/flow` | Flow analysis with SQL injection prevention via `sequence_next_node()` |

All 5 advanced analytics views added to the dashboard.

---

## Phase 4: Production Hardening -- COMPLETE

### Delivered

| Task | Description |
|---|---|
| 4.1 | MaxMind GeoLite2 GeoIP reader with graceful fallback |
| 4.2 | Argon2id dashboard authentication with 256-bit session tokens (HttpOnly cookies) |
| 4.3 | Bot traffic filtering via User-Agent detection |
| 4.4 | API key management (create, list, revoke) with SHA-256 hashed storage |
| 4.5 | CORS hardening: permissive for ingestion, restrictive for dashboard |

---

## Phase 5: Operational Excellence -- COMPLETE

### Delivered

| Task | Description |
|---|---|
| 5.1 | Data retention cleanup with configurable `MALLARD_RETENTION_DAYS` |
| 5.2 | Data export API (`GET /api/stats/export`) with CSV and JSON formats |
| 5.3 | Graceful shutdown with SIGINT/SIGTERM handling and buffered event flush |
| 5.4 | Enhanced health check (`GET /health/detailed`) with JSON system status |
| 5.5 | Structured logging with `MALLARD_LOG_FORMAT=json` option |
| 5.6 | Configuration template (`mallard-metrics.toml.example`) |
| 5.7 | Docker build optimization with dependency caching layer |

---

## Phase 6: Scale and Performance -- COMPLETE

### Delivered

| Task | Description |
|---|---|
| 6.1 | TTL-based in-memory query result cache (`query/cache.rs`) |
| 6.2 | Per-site token-bucket rate limiter (`ingest/ratelimit.rs`) |
| 6.4 | Query benchmarks (core metrics, timeseries, breakdowns) in Criterion suite |
| 6.5 | Prometheus metrics endpoint (`GET /metrics`) |

---

## Future Considerations

These are identified as potential future work but are not currently planned. They depend on real-world production usage data and should only be pursued when actual need is demonstrated.

- **Write-ahead log (WAL)** -- If buffer data loss on crash becomes a concern beyond what graceful shutdown handles
- **Parquet compaction** -- Merge many small Parquet files per partition into fewer large ones for query performance
- **Connection pooling** -- If concurrent query load requires it (currently single connection behind Mutex)
- **Multi-node deployment** -- Only if single-process cannot handle the load (DuckDB is very fast for analytical workloads)
- **Custom dashboard themes** -- User-configurable dashboard appearance
- **Email reports** -- Scheduled analytics summaries via email
- **Webhook notifications** -- Real-time alerts for traffic anomalies

---

## Implementation Summary

### Architecture

```
Phase 1: Core foundation (ingestion, storage, queries, dashboard, CI)
Phase 2: Integration completeness (all backend wired to frontend)
Phase 3: Advanced analytics (behavioral extension features)
Phase 4: Security hardening (auth, GeoIP, CORS, bot filtering)
Phase 5: Operational maturity (retention, export, shutdown, logging)
Phase 6: Performance at scale (caching, rate limiting, metrics)
```

### Critical Path (completed)

```
Phase 2.1 (UA integration)     -> Phase 2.4 (breakdowns have real data)
Phase 2.2 (GeoIP wiring)       -> Phase 4.1 (MaxMind integration)
Phase 2.6 (origin validation)  -> Phase 4.2 (full auth builds on this)
Phase 4.2 (authentication)     -> Phase 4.4 (API keys), Phase 5.2 (export auth)
```

### Verification Protocol

Every phase was verified with:

1. `cargo test` -- all tests pass
2. `cargo clippy --all-targets` -- 0 warnings
3. `cargo fmt -- --check` -- 0 violations
4. `cargo doc --no-deps` -- builds clean
