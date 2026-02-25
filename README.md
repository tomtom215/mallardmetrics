# Mallard Metrics

Self-hosted, privacy-focused web analytics powered by DuckDB and the `behavioral` extension. Single binary. Single process. Zero external dependencies.

A lightweight, GDPR/CCPA-compliant alternative to Plausible Analytics, built in Rust for maximum performance and minimum resource usage.

---

## Table of Contents

- [Features](#features)
- [Quick Start](#quick-start)
- [Tracking Script](#tracking-script)
- [Configuration](#configuration)
- [API Reference](#api-reference)
- [Architecture](#architecture)
- [Dashboard](#dashboard)
- [Technology Stack](#technology-stack)
- [Development](#development)
- [Deployment](#deployment)
- [Documentation Index](#documentation-index)
- [License](#license)

---

## Features

### Privacy by Design

- **No cookies** -- Visitor identification uses a daily-rotating HMAC-SHA256 hash of IP + User-Agent + daily salt
- **No PII storage** -- IP addresses are used only for hashing and GeoIP lookup, then immediately discarded
- **Daily salt rotation** -- Visitor IDs change every day, preventing long-term tracking
- **GDPR/CCPA compliant** -- No personal data stored, no consent banner required

### Single Binary Deployment

- **One process** handles ingestion, storage, querying, authentication, and the dashboard
- **Zero external dependencies** -- DuckDB is embedded, no separate database required
- **`FROM scratch` Docker image** -- Static musl binary with no runtime dependencies

### Analytical Power

- **Core metrics** -- Unique visitors, pageviews, bounce rate, average session duration
- **Breakdowns** -- Pages, referrer sources, browsers, operating systems, devices, countries
- **Time-series** -- Hourly and daily aggregations with chart visualization
- **Funnel analysis** -- Multi-step conversion funnels via `window_funnel()`
- **Retention cohorts** -- Weekly cohort retention grids via `retention()`
- **Session analytics** -- Session duration, pages per session via `sessionize()`
- **Sequence matching** -- Behavioral pattern detection via `sequence_match()`
- **Flow analysis** -- Next-page navigation patterns via `sequence_next_node()`

### Production Ready

- **Argon2id authentication** -- Password-protected dashboard with cryptographic session tokens
- **API key management** -- Programmatic access with SHA-256 hashed keys (`mm_` prefix)
- **Rate limiting** -- Per-site token-bucket rate limiter for ingestion
- **Query caching** -- TTL-based in-memory cache for analytics queries
- **Bot filtering** -- Automatic filtering of known bot User-Agents
- **GeoIP resolution** -- MaxMind GeoLite2 integration with graceful fallback
- **Data retention** -- Configurable automatic cleanup of old Parquet partitions
- **Graceful shutdown** -- Buffered events are flushed before process exit
- **Prometheus metrics** -- `GET /metrics` endpoint for monitoring

---

## Quick Start

### Docker (recommended)

```bash
docker run -p 8000:8000 -v mallard-data:/data ghcr.io/tomtom215/mallard-metrics
```

### Docker Compose

```bash
docker compose up -d
```

The default `docker-compose.yml` includes persistent storage, restart policy, and environment variable configuration. Set `MALLARD_SECRET` and `MALLARD_ADMIN_PASSWORD` in your environment for production use.

### From Source

```bash
# Requires Rust 1.93.0+
cargo build --release
./target/release/mallard-metrics
```

Visit `http://localhost:8000` to access the dashboard. On first visit, you will be prompted to set an admin password.

---

## Tracking Script

Add the following snippet to your website's `<head>` or before `</body>`:

```html
<script defer data-domain="yourdomain.com"
  src="https://your-mallard-instance.com/tracking/script.js"></script>
```

### Custom Events and Revenue Tracking

The tracking script exposes a `window.mallard()` function for custom event tracking:

```javascript
// Track a custom event
window.mallard("signup", {
  props: { plan: "pro", source: "landing-page" }
});

// Track revenue
window.mallard("purchase", {
  revenue: 49.99,
  currency: "USD",
  props: { product: "annual-plan" },
  callback: () => console.log("tracked")
});
```

**Options:**

| Parameter  | Type     | Description                        |
|------------|----------|------------------------------------|
| `props`    | Object   | Custom properties (max 4096 chars) |
| `revenue`  | Number   | Revenue amount                     |
| `currency` | String   | ISO 4217 currency code             |
| `callback` | Function | Called after the event is sent      |

The tracking script is under 1KB minified and has zero external dependencies.

---

## Configuration

Mallard Metrics is configured via a TOML file, environment variables, or both. Environment variables override TOML values.

```bash
# Provide a TOML config file as the first argument
./mallard-metrics /path/to/mallard-metrics.toml
```

See [`mallard-metrics.toml.example`](mallard-metrics.toml.example) for a fully documented template.

### Environment Variables

| Variable | Default | Description |
|---|---|---|
| `MALLARD_HOST` | `0.0.0.0` | Server bind address |
| `MALLARD_PORT` | `8000` | Server listen port |
| `MALLARD_DATA_DIR` | `data` | Directory for Parquet data files |
| `MALLARD_SECRET` | (random) | HMAC secret for visitor ID hashing. **Set this for production** to persist visitor IDs across restarts |
| `MALLARD_ADMIN_PASSWORD` | (none) | Admin password for dashboard authentication |
| `MALLARD_FLUSH_COUNT` | `1000` | Number of events buffered before flushing to disk |
| `MALLARD_FLUSH_INTERVAL` | `60` | Seconds between periodic buffer flushes |
| `MALLARD_GEOIP_DB` | (none) | Path to MaxMind GeoLite2-City.mmdb file |
| `MALLARD_DASHBOARD_ORIGIN` | (none) | Restrict dashboard CORS to this origin |
| `MALLARD_FILTER_BOTS` | `true` | Filter bot traffic from analytics |
| `MALLARD_RETENTION_DAYS` | `0` | Auto-delete data older than N days (0 = unlimited) |
| `MALLARD_SESSION_TTL` | `86400` | Dashboard session TTL in seconds (default: 24h) |
| `MALLARD_SHUTDOWN_TIMEOUT` | `30` | Graceful shutdown timeout in seconds |
| `MALLARD_RATE_LIMIT` | `0` | Max events/sec per site (0 = no limit) |
| `MALLARD_CACHE_TTL` | `60` | Query cache TTL in seconds |
| `MALLARD_LOG_FORMAT` | `text` | Log output format: `text` or `json` |
| `RUST_LOG` | (none) | Log level filter (e.g., `mallard_metrics=info`) |

---

## API Reference

All `/api/stats/*`, `/api/keys/*`, and `/api/stats/export` endpoints require authentication (session cookie or API key). The ingestion endpoint (`/api/event`) and health checks are unauthenticated.

### Common Query Parameters

| Parameter | Default | Description |
|---|---|---|
| `site_id` | (required) | Analytics property identifier |
| `period` | `30d` | Time period: `day`, `today`, `7d`, `30d`, `90d` |
| `start_date` | (none) | Explicit start date (YYYY-MM-DD) |
| `end_date` | (none) | Explicit end date (YYYY-MM-DD) |
| `limit` | `10` | Result limit (breakdowns only) |

### Endpoints

#### Health and Monitoring

| Method | Endpoint | Description |
|---|---|---|
| GET | `/health` | Simple health check (returns `"ok"`) |
| GET | `/health/detailed` | JSON system status (version, buffer, auth, GeoIP, cache) |
| GET | `/metrics` | Prometheus metrics (`text/plain; version=0.0.4`) |

#### Authentication

| Method | Endpoint | Description |
|---|---|---|
| POST | `/auth/setup` | Initial admin password setup (first run only) |
| POST | `/auth/login` | Login with credentials |
| POST | `/auth/logout` | Logout and clear session |
| GET | `/auth/status` | Check authentication status |

#### Ingestion

| Method | Endpoint | Description |
|---|---|---|
| POST | `/api/event` | Ingest a tracking event (permissive CORS) |

#### Core Analytics (authenticated)

| Method | Endpoint | Description |
|---|---|---|
| GET | `/api/stats/main` | Unique visitors, pageviews, bounce rate, avg session duration |
| GET | `/api/stats/timeseries` | Time-bucketed visitor and pageview counts |

#### Breakdowns (authenticated)

| Method | Endpoint | Description |
|---|---|---|
| GET | `/api/stats/breakdown/pages` | Top pages |
| GET | `/api/stats/breakdown/sources` | Top referrer sources |
| GET | `/api/stats/breakdown/browsers` | Browser distribution |
| GET | `/api/stats/breakdown/os` | Operating system distribution |
| GET | `/api/stats/breakdown/devices` | Device type distribution |
| GET | `/api/stats/breakdown/countries` | Country distribution |

#### Advanced Analytics (authenticated, require `behavioral` extension)

| Method | Endpoint | Parameters | Description |
|---|---|---|---|
| GET | `/api/stats/sessions` | -- | Session metrics (total, avg duration, pages/session) |
| GET | `/api/stats/funnel` | `steps`, `window` (default: `1 day`) | Multi-step conversion funnel |
| GET | `/api/stats/retention` | `weeks` (default: 4, range: 1-52) | Retention cohort grid |
| GET | `/api/stats/sequences` | `steps` (min: 2) | Sequence pattern matching |
| GET | `/api/stats/flow` | `page` | Next-page flow analysis |

#### Data Management (authenticated)

| Method | Endpoint | Parameters | Description |
|---|---|---|---|
| GET | `/api/stats/export` | `format` (`csv` or `json`) | Export analytics data |
| POST | `/api/keys` | -- | Create a new API key |
| GET | `/api/keys` | -- | List all API keys |
| DELETE | `/api/keys/{key_hash}` | -- | Revoke an API key |

#### Dashboard

| Method | Endpoint | Description |
|---|---|---|
| GET | `/` | Dashboard SPA (Preact + HTM, embedded in binary) |

---

## Architecture

```
                    +------------------------------------------+
  Tracking Script   |                                          |
  POST /api/event --+--> Ingest Handler                       |
                    |    |  Origin Check  |  Bot Filter        |
                    |    |  Rate Limiter  |  UA Parser         |
                    |    |  GeoIP Lookup  |  Visitor ID        |
                    |    v                                      |
                    |  In-Memory Event Buffer                   |
                    |    |  (flush: threshold / timer / SIGINT) |
                    |    v                                      |
                    |  DuckDB (embedded) <-- behavioral ext    |
                    |    |  hot: events table                   |
                    |    |  COPY TO Parquet (ZSTD)              |
                    |    v                                      |
                    |  data/events/site_id=*/date=*/*.parquet  |
                    |                                          |
                    |  events_all VIEW                         |
                    |    = events (hot) + read_parquet (cold)  |
                    |    |  Cache Layer                        |
                    |    v                                      |
  Dashboard/API  <--+-- Axum Router                           |
                    |    Auth Layer  |  CORS Layer             |
                    +------------------------------------------+
                                Single Process
```

### Module Map

| Module | Purpose |
|---|---|
| `config.rs` | TOML + environment variable configuration |
| `server.rs` | Axum router, middleware, and route registration |
| `ingest/handler.rs` | `POST /api/event` ingestion handler |
| `ingest/buffer.rs` | In-memory event buffer with periodic flush |
| `ingest/visitor_id.rs` | HMAC-SHA256 privacy-safe visitor ID generation |
| `ingest/useragent.rs` | User-Agent parsing (browser, OS, version, device) |
| `ingest/geoip.rs` | MaxMind GeoLite2 reader with graceful fallback |
| `ingest/ratelimit.rs` | Per-site token-bucket rate limiter |
| `storage/schema.rs` | DuckDB table definitions and behavioral extension loading |
| `storage/parquet.rs` | Parquet write, read, and date-partitioning |
| `storage/migrations.rs` | Schema versioning |
| `query/metrics.rs` | Core metric calculations (visitors, pageviews, bounce rate) |
| `query/breakdowns.rs` | Dimension breakdown queries |
| `query/timeseries.rs` | Time-bucketed aggregations |
| `query/sessions.rs` | `sessionize()`-based session queries |
| `query/funnel.rs` | `window_funnel()` conversion funnel builder |
| `query/retention.rs` | `retention()` cohort query execution |
| `query/sequences.rs` | `sequence_match()` pattern query execution |
| `query/flow.rs` | `sequence_next_node()` flow analysis |
| `query/cache.rs` | TTL-based query result cache |
| `api/stats.rs` | All analytics API handlers |
| `api/errors.rs` | API error types and HTTP responses |
| `api/auth.rs` | Origin validation, session auth, API key management |
| `dashboard/` | Embedded SPA (Preact + HTM, no build step) |

---

## Dashboard

The dashboard is a single-page application built with Preact + HTM and embedded directly in the binary via `rust-embed`. No build step or Node.js required.

**Views include:**

- Real-time visitor and pageview counts
- Time-series line chart (visitors and pageviews)
- Six breakdown tables (pages, sources, browsers, OS, devices, countries)
- Session analytics cards
- Funnel analysis visualization
- Retention cohort grid
- Sequence matching metrics
- Flow analysis (next-page navigation)

---

## Technology Stack

| Component | Technology | Version |
|---|---|---|
| Language | Rust | 1.93.0 (MSRV) |
| Web Framework | Axum | 0.8 |
| Database | DuckDB (embedded) | 1.4.4 |
| Analytics Engine | `behavioral` extension | runtime-loaded |
| Storage Format | Parquet (ZSTD compressed) | date-partitioned |
| Frontend | Preact + HTM | no build step |
| Password Hashing | Argon2id | `argon2` 0.5 |
| GeoIP | MaxMind GeoLite2 | `maxminddb` 0.27 |
| Deployment | Static musl binary | `FROM scratch` Docker |

---

## Development

### Prerequisites

- Rust 1.93.0+ (managed automatically via `rust-toolchain.toml`)
- Git

### Build and Test

```bash
# Build
cargo build

# Run all tests (280 total: 219 unit + 61 integration)
cargo test

# Clippy (zero warnings required)
cargo clippy --all-targets

# Format check
cargo fmt -- --check

# Build documentation
cargo doc --no-deps

# Run the server
cargo run

# Run benchmarks
cargo bench
```

### Quality Standards

- **Zero clippy warnings** -- pedantic, nursery, and cargo lint groups enabled
- **Zero formatting violations** -- enforced via `cargo fmt`
- **All 280 tests pass** -- no ignored tests
- **Documentation builds without errors**

See [CONTRIBUTING.md](CONTRIBUTING.md) for the full development workflow.

---

## Deployment

### Docker Compose (recommended for production)

Create a `.env` file:

```bash
MALLARD_SECRET=your-random-secret-here
MALLARD_ADMIN_PASSWORD=your-admin-password
```

Then run:

```bash
docker compose up -d
```

### Reverse Proxy

Place Mallard Metrics behind nginx, Caddy, or a similar reverse proxy for TLS termination:

```nginx
# nginx example
server {
    listen 443 ssl;
    server_name analytics.example.com;

    location / {
        proxy_pass http://127.0.0.1:8000;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

### GeoIP Setup (optional)

1. Register for a free MaxMind account at [maxmind.com](https://www.maxmind.com/en/geolite2/signup)
2. Download the GeoLite2-City database (`.mmdb` format)
3. Set `MALLARD_GEOIP_DB=/path/to/GeoLite2-City.mmdb`

Country-level resolution works without GeoIP. The system degrades gracefully if the database is missing.

---

## Documentation Index

| Document | Description |
|---|---|
| [README.md](README.md) | Project overview, quick start, API reference |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Development setup, workflow, code standards, PR checklist |
| [SECURITY.md](SECURITY.md) | Security model, privacy guarantees, threat model, vulnerability reporting |
| [CHANGELOG.md](CHANGELOG.md) | Version history following Keep a Changelog format |
| [ROADMAP.md](ROADMAP.md) | Implementation phases, completed work, and future plans |
| [PERF.md](PERF.md) | Benchmark framework, methodology, and performance engineering |
| [LESSONS.md](LESSONS.md) | Development lessons learned, organized by category |
| [CLAUDE.md](CLAUDE.md) | AI assistant session protocol and project metadata |
| [mallard-metrics.toml.example](mallard-metrics.toml.example) | Annotated configuration template |

---

## License

MIT
