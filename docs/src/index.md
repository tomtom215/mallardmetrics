# Mallard Metrics

**Self-hosted, privacy-focused web analytics powered by DuckDB and the `behavioral` extension.**

Single binary. Single process. Zero external dependencies.

---

## What is Mallard Metrics?

Mallard Metrics is a lightweight, GDPR/CCPA-compliant alternative to cloud analytics platforms. It runs entirely on your infrastructure, stores no personally identifiable information, and requires no cookies or consent banners.

Built in Rust for predictable, low resource usage. The embedded DuckDB database — combined with the `behavioral` extension — provides SQL-native behavioral analytics: funnels, retention cohorts, session analysis, sequence matching, and flow analysis. No third-party services involved.

## Core Properties

| Property | Value |
|---|---|
| Language | Rust (MSRV 1.93.0) |
| Web framework | Axum 0.8.x |
| Database | DuckDB (disk-based, embedded, in-process) |
| Analytics | `behavioral` extension (loaded at runtime) |
| Storage | Date-partitioned Parquet files (ZSTD-compressed) |
| Frontend | Preact + HTM (no build step, embedded in binary) |
| Deployment | Static musl binary, `FROM scratch` Docker image |
| Tests | 311 passing (249 unit + 62 integration) |

## Key Features

### Privacy by Design

- **No cookies** — Visitor identification uses a daily-rotating HMAC-SHA256 hash of `IP + User-Agent + daily salt`.
- **No PII storage** — IP addresses are hashed and discarded; they are never written to disk.
- **Daily salt rotation** — Visitor IDs change every 24 hours, preventing long-term tracking.
- **GDPR/CCPA compliant** — No personal data stored. No consent banner required.

### Single Binary Deployment

- One process handles ingestion, storage, querying, authentication, and the dashboard.
- DuckDB is embedded — no separate database to install or operate.
- `FROM scratch` Docker image: the binary is the only file in the container.
- WAL-based durability: disk-backed DuckDB preserves hot events through crashes.

### Analytical Power

| Category | Capabilities |
|---|---|
| Core metrics | Unique visitors, pageviews, bounce rate, pages/session |
| Breakdowns | Pages, referrers, browsers, OS, devices, countries |
| Time-series | Hourly and daily aggregations |
| Funnel analysis | Multi-step conversion funnels via `window_funnel()` |
| Retention cohorts | Weekly retention grids via `retention()` |
| Session analytics | Duration, depth via `sessionize()` |
| Sequence matching | Behavioral patterns via `sequence_match()` |
| Flow analysis | Next-page navigation via `sequence_next_node()` |

### Production Ready

- **Argon2id authentication** — Password-protected dashboard with cryptographic session tokens.
- **API key management** — Programmatic access with SHA-256 hashed keys (`mm_` prefix), disk-persisted.
- **Rate limiting** — Per-site token-bucket rate limiter on the ingestion endpoint.
- **Query caching** — TTL-based in-memory cache for analytics queries.
- **Bot filtering** — Automatic filtering of known bot User-Agents.
- **GeoIP** — MaxMind GeoLite2 integration with graceful fallback.
- **Data retention** — Configurable automatic cleanup of old Parquet partitions.
- **Graceful shutdown** — Buffered events are flushed before process exit.
- **Prometheus metrics** — `GET /metrics` for scraping with counters for ingestion, auth, cache, and rate limiting.
- **OWASP security headers** — Including HSTS, CSP, Permissions-Policy, and X-Request-ID.
- **CSRF protection** — Origin/Referer validation on all state-mutating endpoints.
- **Brute-force protection** — Per-IP login lockout with configurable threshold and lockout duration.

## When Should You Use Mallard Metrics?

Mallard Metrics is a good fit when you:

- Want full control over your analytics data on your own server.
- Need GDPR/CCPA compliance without third-party data processors.
- Are running a small-to-medium website and want low operational overhead.
- Need advanced behavioral analytics (funnels, retention, sequences) without a SaaS subscription.
- Want to demonstrate the power of DuckDB's `behavioral` extension in a real-world production context.

It is **not** designed for:

- Multi-region distributed analytics at very high volume (millions of events/minute).
- Real-time dashboards with sub-second latency requirements.
- Replacing a full data warehouse.

## Project Status

Mallard Metrics is actively developed and production-ready. See [GitHub](https://github.com/tomtom215/mallardmetrics) for the latest releases and issue tracker.

The `behavioral` extension powering advanced analytics is developed at [github.com/tomtom215/duckdb-behavioral](https://github.com/tomtom215/duckdb-behavioral).
