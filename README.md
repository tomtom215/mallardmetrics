# Mallard Metrics

Self-hosted, privacy-focused web analytics powered by DuckDB and the `behavioral` extension. Single binary. Single process. Zero external dependencies.

## Features

- **Privacy-first**: No cookies. Visitor ID = daily-rotating hash of IP + User-Agent. IP discarded after hashing. GDPR/CCPA compliant by design.
- **Single binary**: One process handles ingestion, storage, querying, and the dashboard.
- **DuckDB + Parquet**: Embedded analytical database with date-partitioned Parquet storage. No external database required.
- **Behavioral analytics**: Funnel analysis, retention cohorts, session analysis, sequence patterns, and flow analysis via the `behavioral` DuckDB extension.
- **Lightweight**: Target <20MB container image, <200MB memory footprint.

## Quick Start

### Docker

```bash
docker run -p 8000:8000 -v mallard-data:/data ghcr.io/tomtom215/mallard-metrics
```

### Docker Compose

```bash
docker compose up -d
```

### From Source

```bash
cargo build --release
./target/release/mallard-metrics
```

## Tracking Script

Add to your website:

```html
<script defer data-domain="yourdomain.com" src="https://your-mallard-instance.com/tracking/script.js"></script>
```

## Configuration

Configuration via TOML file or environment variables:

| Environment Variable | Default | Description |
|---|---|---|
| `MALLARD_HOST` | `0.0.0.0` | Listen address |
| `MALLARD_PORT` | `8000` | Listen port |
| `MALLARD_DATA_DIR` | `data` | Data directory for Parquet files |
| `MALLARD_SECRET` | (random) | Secret for visitor ID hashing (set for persistence across restarts) |
| `MALLARD_FLUSH_COUNT` | `1000` | Events buffered before flush |
| `MALLARD_FLUSH_INTERVAL` | `60` | Seconds between periodic flushes |

## API Endpoints

| Endpoint | Method | Description |
|---|---|---|
| `/health` | GET | Health check |
| `/api/event` | POST | Event ingestion |
| `/api/stats/main` | GET | Core metrics (visitors, pageviews, bounce rate) |
| `/api/stats/timeseries` | GET | Time-bucketed data |
| `/api/stats/breakdown/pages` | GET | Top pages |
| `/api/stats/breakdown/sources` | GET | Top referrer sources |
| `/api/stats/breakdown/browsers` | GET | Browser breakdown |
| `/api/stats/breakdown/os` | GET | OS breakdown |
| `/api/stats/breakdown/devices` | GET | Device type breakdown |
| `/api/stats/breakdown/countries` | GET | Country breakdown |
| `/` | GET | Dashboard SPA |

## Technology Stack

| Component | Technology |
|---|---|
| Language | Rust (1.85.0) |
| Web Framework | Axum 0.8 |
| Database | DuckDB (embedded) |
| Analytics | `behavioral` extension |
| Storage | Parquet (date-partitioned) |
| Frontend | Preact + HTM |
| Deployment | Static musl binary |

## Development

```bash
# Run tests
cargo test

# Lint
cargo clippy --all-targets

# Format
cargo fmt

# Build docs
cargo doc --no-deps
```

## License

MIT
