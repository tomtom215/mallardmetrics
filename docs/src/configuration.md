# Configuration

Mallard Metrics is configured through a TOML file and two environment variables. All settings have sensible defaults; you can start without any configuration file.

## Loading Configuration

Pass the path to a TOML file as the first command-line argument:

```bash
mallard-metrics /etc/mallard-metrics/config.toml
```

If no argument is provided, defaults are used.

## Environment Variables

These two values are secrets and must not be stored in files committed to source control. Set them in your shell or a `.env` file:

| Variable | Required | Description |
|---|---|---|
| `MALLARD_SECRET` | Recommended | 32+ character random string used as HMAC key for visitor ID hashing. If unset, a random value is generated on each start (visitor IDs will change across restarts). |
| `MALLARD_ADMIN_PASSWORD` | Recommended | Dashboard password. If unset, the dashboard is unauthenticated. |
| `MALLARD_LOG_FORMAT` | Optional | Set to `json` for structured JSON log output. Omit or set to any other value for human-readable text logs. |

## TOML Configuration Reference

A complete example is shipped as `mallard-metrics.toml.example`. Every field has a default and is optional.

```toml
# Network binding
host = "0.0.0.0"   # default
port = 8000         # default

# Storage
data_dir = "data"   # relative or absolute path; events and Parquet files are stored here

# Event buffer
flush_event_count = 1000   # flush buffer to Parquet when this many events accumulate
flush_interval_secs = 60   # also flush on this interval (seconds)

# Site allowlist — leave empty to accept events from any origin
# site_ids = ["example.com", "other-site.org"]
site_ids = []

# GeoIP database (optional — gracefully skipped if missing)
# geoip_db_path = "/path/to/GeoLite2-City.mmdb"

# Dashboard CORS origin (optional — set when dashboard is on a different origin)
# dashboard_origin = "https://analytics.example.com"

# Bot filtering (default: true — filters known bot User-Agents from event ingestion)
filter_bots = true

# Data retention: delete Parquet partitions older than this many days
# Set to 0 for unlimited retention (default)
retention_days = 0

# Session authentication TTL in seconds (default: 86400 = 24 hours)
session_ttl_secs = 86400

# Graceful shutdown timeout in seconds (default: 30)
shutdown_timeout_secs = 30

# Ingestion rate limit per site_id (events/second, 0 = unlimited)
rate_limit_per_site = 0

# Query cache TTL in seconds (0 = no caching, default: 60)
cache_ttl_secs = 60

# Log format: "text" (default) or "json"
log_format = "text"
```

## Configuration Field Details

### `host` / `port`

The address and port the HTTP server listens on.

- Default: `0.0.0.0:8000`
- To restrict to localhost: `host = "127.0.0.1"`

### `data_dir`

Root directory for all persistent data. Mallard Metrics creates subdirectories:

```
data/
└── events/
    └── site_id=example.com/
        └── date=2024-01-15/
            ├── 0001.parquet
            └── 0002.parquet
```

Parquet files are ZSTD-compressed. The directory is created automatically.

### `flush_event_count` / `flush_interval_secs`

Events arrive into a memory buffer before being flushed to Parquet. Flushing happens when either threshold is reached. The buffer is also flushed on graceful shutdown.

- Lower values reduce data loss on crash; higher values reduce I/O.
- Queries always see both buffered (hot) and persisted (cold) data via the `events_all` view.

### `site_ids`

An allowlist of site identifiers. If non-empty, the `Origin` header of each ingestion request must exactly match one of the listed values. Requests from unlisted origins receive a `403 Forbidden` response.

The comparison is **exact**: `example.com` matches `https://example.com` and `http://example.com:8080` (with explicit port) but not `example.com.other.io`.

### `geoip_db_path`

Path to a MaxMind GeoLite2-City `.mmdb` file. GeoLite2 databases are free for non-commercial use and available at [maxmind.com](https://www.maxmind.com/en/geolite2/signup).

If the file is not specified or does not exist, country/region/city fields are stored as `NULL`. This is the default behavior and does not cause any errors.

### `rate_limit_per_site`

Maximum events per second accepted per `site_id`. Uses a token-bucket algorithm. Set to `0` (default) for no limit.

### `cache_ttl_secs`

Query results for `/api/stats/main` and `/api/stats/timeseries` are cached in memory for this duration. Setting to `0` disables caching (useful for development). Default is 60 seconds.

### `retention_days`

Parquet partition directories older than `retention_days` days are deleted automatically by a background task that runs daily. Set to `0` (default) for unlimited retention.
