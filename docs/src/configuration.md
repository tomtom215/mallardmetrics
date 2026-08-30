# Configuration

Mallard Metrics is configured through a TOML file and environment variables. All settings have sensible defaults; you can start without any configuration file.

## Loading Configuration

Pass the path to a TOML file as the first command-line argument:

```bash
mallard-metrics /etc/mallard-metrics/config.toml
```

With no argument, defaults apply.

Two loading rules are worth knowing:

- **A named file that cannot be read or parsed is a startup error.** It is not
  silently replaced with defaults, because that could quietly disable retention
  or drop the site allowlist with nothing but a log line to say so.
- **Unknown keys are rejected.** A typo such as `retention_dayz = 30` fails at
  startup rather than leaving the real setting at its default while appearing
  configured.

Environment variables override file values. Validation runs before the server
binds, so a malformed `dashboard_origin` or `geoip_precision` is reported
immediately rather than surfacing on the first request.

## Environment Variables

These two values are secrets and must not be stored in files committed to source control. Set them in your shell or a `.env` file:

| Variable | Required | Description |
|---|---|---|
| `MALLARD_SECRET` | Recommended | HMAC key for visitor ID hashing. If unset, a UUID is auto-generated on first start and persisted to `data_dir/.secret` (survives restarts). Set explicitly in production for portability across hosts. |
| `MALLARD_ADMIN_PASSWORD` | Recommended | Dashboard password. If unset, the dashboard is unauthenticated. |
| `MALLARD_MAX_LOGIN_ATTEMPTS` | Optional | Override `max_login_attempts` at runtime. |
| `MALLARD_LOGIN_LOCKOUT` | Optional | Override `login_lockout_secs` at runtime. |
| `MALLARD_LOG_FORMAT` | Optional | Set to `json` for structured JSON log output. Omit or set to any other value for human-readable text logs. |
| `MALLARD_SECURE_COOKIES` | Optional | Set to `true` to add the `Secure` flag to session cookies (required behind TLS). |
| `MALLARD_METRICS_TOKEN` | Optional | Bearer token protecting the `/metrics` endpoint. |
| `MALLARD_GEOIP_DB` | Optional | Path to MaxMind GeoLite2-City `.mmdb` file. |
| `MALLARD_DASHBOARD_ORIGIN` | Optional | Restrict dashboard CORS and enable CSRF protection. |
| `MALLARD_MAX_CONCURRENT_QUERIES` | Optional | Max concurrent analytical queries (default 10). Returns 429 when exhausted. |
| `MALLARD_CACHE_MAX_ENTRIES` | Optional | Max query cache entries (default 10000). |
| `MALLARD_GDPR_MODE` | Optional | Enable GDPR-friendly preset (see [PRIVACY.md](https://github.com/tomtom215/mallardmetrics/blob/main/PRIVACY.md)). |
| `MALLARD_GEOIP_PRECISION` | Optional | GeoIP precision: `city`, `region`, `country`, or `none`. |
| `MALLARD_HOST` | Optional | Server bind address (default `0.0.0.0`). |
| `MALLARD_PORT` | Optional | Server listen port (default `8000`). |
| `MALLARD_DATA_DIR` | Optional | Data directory for Parquet files and DuckDB (default `data`). |
| `MALLARD_FLUSH_COUNT` | Optional | Events buffered before flushing to disk (default `1000`). |
| `MALLARD_FLUSH_INTERVAL` | Optional | Seconds between periodic buffer flushes (default `60`). |
| `MALLARD_FILTER_BOTS` | Optional | Filter known bot User-Agents (default `true`). |
| `MALLARD_RETENTION_DAYS` | Optional | Auto-delete data older than N days; 0 = unlimited (default `0`). |
| `MALLARD_SESSION_TTL` | Optional | Dashboard session TTL in seconds (default `86400`). |
| `MALLARD_SHUTDOWN_TIMEOUT` | Optional | Graceful shutdown timeout in seconds (default `30`). |
| `MALLARD_RATE_LIMIT` | Optional | Max events/sec per site; 0 = unlimited (default `0`). |
| `MALLARD_CACHE_TTL` | Optional | Query cache TTL in seconds (default `60`). |
| `MALLARD_STRIP_REFERRER_QUERY` | Optional | Strip query/fragment from stored referrers (default `false`). |
| `MALLARD_ROUND_TIMESTAMPS` | Optional | Round timestamps to the nearest hour (default `false`). |
| `MALLARD_SUPPRESS_VISITOR_ID` | Optional | Replace HMAC hash with per-request UUID (default `false`). |
| `MALLARD_SUPPRESS_BROWSER_VERSION` | Optional | Store browser name only (default `false`). |
| `MALLARD_SUPPRESS_OS_VERSION` | Optional | Store OS name only (default `false`). |
| `MALLARD_SUPPRESS_SCREEN_SIZE` | Optional | Omit screen width and device type (default `false`). |
| `MALLARD_SITE_IDS` | Optional | Comma-separated site allowlist, enforced against the Origin header *and* the event payload. |
| `MALLARD_TRUST_PROXY_HEADERS` | Optional | Trust `X-Forwarded-For` / `X-Real-IP` (default `false`). See below. |
| `MALLARD_VISITOR_SALT_ROTATION_DAYS` | Optional | Days a visitor-ID salt stays valid (default `1`). See below. |
| `MALLARD_SESSION_WINDOW_MINUTES` | Optional | Inactivity gap that ends a session (default `30`). |
| `MALLARD_REALTIME_WINDOW_MINUTES` | Optional | Window the realtime endpoint treats as "now" (default `5`). |
| `MALLARD_COMPACT_AFTER_FILES` | Optional | Merge a partition's Parquet files past this count; 0 disables (default `24`). |
| `MALLARD_READ_CONNECTIONS` | Optional | Read-only DuckDB connections for queries (default `4`). |
| `MALLARD_MAX_BUFFERED_EVENTS` | Optional | Cap on buffered events; 0 = unbounded (default `100000`). |
| `MALLARD_MAX_TRACKED_KEYS` | Optional | Cap on rate-limit buckets and login-attempt records (default `10000`). |
| `MALLARD_MAX_SESSIONS` | Optional | Cap on concurrent dashboard sessions (default `10000`). |
| `MALLARD_RATE_LIMIT_PER_IP` | Optional | Max events/sec per client IP; 0 = unlimited (default `0`). |

Boolean variables accept `1/true/yes/on` and `0/false/no/off`, case-insensitively.
Anything else logs a warning and leaves the setting unchanged, rather than being
read as "true" because it is not the literal string `false`.

## Two settings that need a decision

### `trust_proxy_headers`

Defaults to `false`, which means the client address comes from the connection.

Turn it on **only** when every request reaches the server through a reverse
proxy that overwrites `X-Forwarded-For` (the bundled Caddy config does). On a
directly reachable server those headers are attacker-controlled, and trusting
them lets any client pick its own visitor ID, geolocation and rate-limit bucket.

Leaving it off on a proxied deployment is also wrong, just less dangerously: every
visitor is then attributed to the proxy's address.

Even when the headers are trusted, their value must parse as an IP address —
with or without a port, bracketed or not. Anything else falls through to the
peer address, so a request that reaches the server without traversing the proxy
cannot inject arbitrary text into the rate-limit key, the GeoIP lookup or the
visitor-ID input. The parsed form is also canonical, so `::1` and
`0:0:0:0:0:0:0:1` are one visitor rather than two.

### `visitor_salt_rotation_days`

Defaults to `1`, the most privacy-protective setting — and the one that limits
what can be measured. A visitor seen on two days carries two unrelated IDs, so:

- "Unique visitors" over a multi-day range counts visitor-days, not people.
- Weekly retention cohorts are structurally impossible;
  `/api/stats/retention` reports this in a `caveat` field.
- A visit spanning UTC midnight is recorded as two visits.

Raise it (7, 30) if you need cross-day analysis, and record the reasoning: it
lengthens the life of a pseudonymous identifier, which is a privacy decision.
Retention over *N* weeks needs at least `(N - 1) * 7` days.

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

# Brute-force protection: lock out an IP after this many failed login attempts (0 = disabled)
max_login_attempts = 5

# Duration in seconds to lock out an IP after exceeding max_login_attempts
login_lockout_secs = 300

# Graceful shutdown timeout in seconds (default: 30)
shutdown_timeout_secs = 30

# Ingestion rate limit per site_id (events/second, 0 = unlimited)
rate_limit_per_site = 0

# Query cache TTL in seconds (0 = no caching, default: 60)
cache_ttl_secs = 60

# Log format: "text" (default) or "json"
log_format = "text"

# Query cache max entries (0 = unlimited, default: 10000)
cache_max_entries = 10000

# Max concurrent analytics queries (0 = unlimited, default: 10)
# Excess requests receive HTTP 429
max_concurrent_queries = 10

# Cookie Secure flag (set to true when behind TLS)
secure_cookies = false

# ── GDPR / Privacy Flags ──────────────────────────────────────────────
# gdpr_mode = false            # convenience preset — enables all flags below
# strip_referrer_query = false  # strip ?query and #fragment from referrers
# round_timestamps = false      # round timestamps to the nearest hour
# suppress_visitor_id = false   # replace HMAC hash with per-request UUID
# suppress_browser_version = false
# suppress_os_version = false
# suppress_screen_size = false
# geoip_precision = "city"      # city | region | country | none
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

### `max_login_attempts` / `login_lockout_secs`

Brute-force protection for the dashboard login endpoint. After `max_login_attempts` consecutive failures from the same IP, that IP is blocked for `login_lockout_secs` seconds. The server responds with `429 Too Many Requests` and a `Retry-After` header during the lockout period.

- `max_login_attempts`: Default `5`. Set to `0` to disable brute-force protection entirely.
- `login_lockout_secs`: Default `300` (5 minutes).

These can also be set via `MALLARD_MAX_LOGIN_ATTEMPTS` and `MALLARD_LOGIN_LOCKOUT` environment variables.
