# Deployment

## Production Checklist

Before going to production:

- [ ] Set `MALLARD_SECRET` to a random 32+ character string and keep it constant across restarts.
- [ ] Set `MALLARD_ADMIN_PASSWORD` to a strong password.
- [ ] Set `MALLARD_SECURE_COOKIES=true` when behind a TLS-terminating reverse proxy so session cookies carry the `Secure` flag.
- [ ] Set `MALLARD_METRICS_TOKEN` to a secret token if the `/metrics` endpoint is publicly reachable.
- [ ] Configure a TLS-terminating reverse proxy (nginx, Caddy, Traefik).
- [ ] Mount a persistent volume for `data_dir` (contains `mallard.duckdb` and Parquet files).
- [ ] Set `site_ids` to restrict event ingestion to your domains.
- [ ] Configure `retention_days` to match your data retention policy.
- [ ] Set `dashboard_origin` to your dashboard URL to enable CSRF protection.
- [ ] Use `/health/ready` as your container or load-balancer readiness probe.

**EU / GDPR deployments — additional steps:**

- [ ] Set `MALLARD_GDPR_MODE=true` (or enable individual flags) to reduce data collection surface.
- [ ] Set `MALLARD_RETENTION_DAYS=30` (or your DPA-approved retention period) for Art. 5(1)(e) storage limitation compliance.
- [ ] Set `MALLARD_GEOIP_PRECISION=country` (already forced by `gdpr_mode`; document it explicitly in your DPIA).
- [ ] Document your legal basis for processing in a DPIA or privacy notice. See [PRIVACY.md](../../../PRIVACY.md) for the full analysis.
- [ ] Use `DELETE /api/gdpr/erase?site_id=...&start_date=...&end_date=...` (Admin API key required) to honour Art. 17 erasure requests.

---

## Docker (Recommended)

### Pull and Run

```bash
docker run -d \
  --name mallard-metrics \
  --restart unless-stopped \
  -p 127.0.0.1:8000:8000 \
  -v mallard-data:/data \
  -e MALLARD_SECRET=your-random-32-char-secret \
  -e MALLARD_ADMIN_PASSWORD=your-dashboard-password \
  -e MALLARD_SECURE_COOKIES=true \
  -e MALLARD_METRICS_TOKEN=your-prometheus-token \
  ghcr.io/tomtom215/mallard-metrics
```

The image is built `FROM scratch` with a static musl binary. It has no shell, no package manager, and no runtime dependencies.

### With a Config File

```bash
docker run -d \
  --name mallard-metrics \
  -v mallard-data:/data \
  -v /etc/mallard-metrics/config.toml:/config.toml:ro \
  -e MALLARD_SECRET=... \
  -e MALLARD_ADMIN_PASSWORD=... \
  ghcr.io/tomtom215/mallard-metrics /config.toml
```

---

## Docker Compose

Save the following as `docker-compose.yml`:

```yaml
services:
  mallard-metrics:
    image: ghcr.io/tomtom215/mallard-metrics:latest
    restart: unless-stopped
    ports:
      - "127.0.0.1:8000:8000"
    volumes:
      - mallard-data:/data
    environment:
      MALLARD_SECRET: "${MALLARD_SECRET}"
      MALLARD_ADMIN_PASSWORD: "${MALLARD_ADMIN_PASSWORD}"
      MALLARD_SECURE_COOKIES: "true"
      MALLARD_METRICS_TOKEN: "${MALLARD_METRICS_TOKEN}"
      MALLARD_LOG_FORMAT: "json"

volumes:
  mallard-data:
```

Create a `.env` file (do not commit to source control):

```bash
MALLARD_SECRET=your-random-32-char-secret
MALLARD_ADMIN_PASSWORD=your-dashboard-password
MALLARD_METRICS_TOKEN=your-prometheus-bearer-token
```

Start:

```bash
docker compose up -d
docker compose logs -f
```

---

## Behind a Reverse Proxy

Mallard Metrics binds to `127.0.0.1:8000` by default when run locally. Configure your reverse proxy to forward requests.

### nginx

```nginx
server {
    listen 443 ssl;
    server_name analytics.example.com;

    ssl_certificate     /etc/ssl/certs/analytics.example.com.crt;
    ssl_certificate_key /etc/ssl/private/analytics.example.com.key;

    location / {
        proxy_pass http://127.0.0.1:8000;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

> **Important:** Mallard Metrics reads the client IP for visitor ID hashing. If behind a proxy, the `X-Forwarded-For` or `X-Real-IP` header must be set correctly. Configure your proxy to send the real client IP.

### Caddy

```caddy
analytics.example.com {
    reverse_proxy 127.0.0.1:8000
}
```

Caddy sets `X-Forwarded-For` automatically.

### After-Proxy Configuration

Once behind a TLS reverse proxy, set these environment variables:

```bash
# Enables Secure flag on session cookies
MALLARD_SECURE_COOKIES=true

# Restricts dashboard CORS and enables CSRF protection
MALLARD_DASHBOARD_ORIGIN=https://analytics.example.com
```

---

## Health and Readiness Probes

| Endpoint | Purpose |
|---|---|
| `GET /health` | Liveness probe — returns `ok` if the process is alive |
| `GET /health/ready` | Readiness probe — queries DuckDB; returns 503 if the database is not ready |
| `GET /health/detailed` | JSON health report — version, buffer, auth, GeoIP, behavioral extension, cache status |

### Kubernetes Example

```yaml
livenessProbe:
  httpGet:
    path: /health
    port: 8000
  initialDelaySeconds: 5
  periodSeconds: 10

readinessProbe:
  httpGet:
    path: /health/ready
    port: 8000
  initialDelaySeconds: 10
  periodSeconds: 15
  failureThreshold: 3
```

### Docker Compose Health Check

The `FROM scratch` image has no shell or utilities (`wget`, `curl`). Use Docker's `HEALTHCHECK` with an external check from the host, or rely on your reverse proxy or orchestrator's health probes:

```bash
# External health check from the host
curl -sf http://localhost:8000/health/ready || exit 1
```

---

## Build from Source (Static musl Binary)

To build a `FROM scratch`-compatible static binary:

```bash
# Install the musl target
rustup target add x86_64-unknown-linux-musl

# Build
cargo build --release --target x86_64-unknown-linux-musl

# The binary
ls -lh target/x86_64-unknown-linux-musl/release/mallard-metrics
```

The resulting binary has no dynamic library dependencies:

```bash
ldd target/x86_64-unknown-linux-musl/release/mallard-metrics
# not a dynamic executable
```

---

## GeoIP Setup

Mallard Metrics supports optional IP geolocation via MaxMind GeoLite2.

1. Create a free account at [maxmind.com](https://www.maxmind.com/en/geolite2/signup).
2. Download the `GeoLite2-City.mmdb` database.
3. Configure the path:

```toml
# config.toml
geoip_db_path = "/data/GeoLite2-City.mmdb"
```

Or with Docker:

```bash
docker run ... \
  -v /path/to/GeoLite2-City.mmdb:/data/GeoLite2-City.mmdb:ro \
  -e ... \
  ghcr.io/tomtom215/mallard-metrics
```

If the file is missing or unreadable, country/region/city fields are stored as `NULL`. No error is raised.

> **Note:** The MaxMind GeoLite2 database is updated monthly. Automate downloads with [geoipupdate](https://github.com/maxmind/geoipupdate).

---

## GDPR-Friendly Deployment

Mallard Metrics provides a configurable privacy mode designed to reduce the data-collection surface to a level that makes aggregate analytics possible under GDPR Art. 6(1)(f) legitimate interests (no consent required) for many EU operators. Consult your legal team; requirements vary by context and member-state law.

### Activate GDPR Mode

The quickest path is the `MALLARD_GDPR_MODE=true` preset, which bundles the recommended privacy settings:

```bash
docker run -d \
  --name mallard-metrics \
  --restart unless-stopped \
  -p 127.0.0.1:8000:8000 \
  -v mallard-data:/data \
  -e MALLARD_SECRET=your-random-32-char-secret \
  -e MALLARD_ADMIN_PASSWORD=your-dashboard-password \
  -e MALLARD_SECURE_COOKIES=true \
  -e MALLARD_GDPR_MODE=true \
  -e MALLARD_RETENTION_DAYS=30 \
  ghcr.io/tomtom215/mallard-metrics
```

Or via TOML config:

```toml
gdpr_mode      = true
retention_days = 30
```

### What GDPR Mode Does

| Flag | Standard | GDPR Mode |
|---|---|---|
| Referrer stored as | Full URL (with query/fragment) | Path only — `?q=...` and `#...` stripped |
| Timestamps | Millisecond precision | Rounded to nearest hour |
| Browser info | Name + version | Name only (e.g. `"Chrome"`) |
| OS info | Name + version | Name only (e.g. `"Windows"`) |
| Screen / device | Stored | Omitted |
| GeoIP | City-level | Country-level only |

### Fine-Grained Privacy Flags

Each setting can be controlled independently via environment variable or TOML key:

| Env var | TOML key | Default | Effect |
|---|---|---|---|
| `MALLARD_GDPR_MODE` | `gdpr_mode` | `false` | Enable all flags below (except suppress_visitor_id) |
| `MALLARD_STRIP_REFERRER_QUERY` | `strip_referrer_query` | `false` | Strip `?query` and `#fragment` from referrers |
| `MALLARD_ROUND_TIMESTAMPS` | `round_timestamps` | `false` | Round timestamps to nearest hour |
| `MALLARD_SUPPRESS_BROWSER_VERSION` | `suppress_browser_version` | `false` | Store browser name only |
| `MALLARD_SUPPRESS_OS_VERSION` | `suppress_os_version` | `false` | Store OS name only |
| `MALLARD_SUPPRESS_SCREEN_SIZE` | `suppress_screen_size` | `false` | Omit screen size and device type |
| `MALLARD_GEOIP_PRECISION` | `geoip_precision` | `"city"` | `"city"` / `"region"` / `"country"` / `"none"` |
| `MALLARD_SUPPRESS_VISITOR_ID` | `suppress_visitor_id` | `false` | Replace HMAC hash with random UUID per request (**breaks unique-visitor counting**) |

> **Note on `suppress_visitor_id`:** This flag is intentionally *not* activated by `gdpr_mode` because it eliminates unique-visitor metrics entirely. The default HMAC-SHA256 visitor ID is pseudonymous personal data under GDPR Recital 26. Most operators can rely on Art. 6(1)(f) legitimate interests for aggregate analytics without suppressing visitor IDs.

### Right to Erasure (Art. 17)

Mallard Metrics supports data erasure requests via an authenticated API endpoint:

```bash
# Requires an Admin API key
curl -X DELETE \
  "https://analytics.example.com/api/gdpr/erase?site_id=mysite.com&start_date=2024-01-01&end_date=2024-12-31" \
  -H "X-API-Key: mm_your_admin_key"
```

Response:

```json
{
  "site_id": "mysite.com",
  "start_date": "2024-01-01",
  "end_date": "2024-12-31",
  "db_events_deleted": 1423,
  "parquet_partitions_removed": 8
}
```

**Important limitations:**
- Erasure is by site and date range, not by individual visitor ID (visitor IDs are pseudonymous hashes and cannot be reverse-mapped to individuals).
- After erasure, the `events_all` VIEW is refreshed automatically.
- Consider setting `MALLARD_RETENTION_DAYS=30` for automated data minimisation under Art. 5(1)(e) in place of manual erasure requests.

---

## Graceful Shutdown

Mallard Metrics handles `SIGINT` (Ctrl+C) and `SIGTERM` (Docker stop, systemd stop). On receiving either signal:

1. The server stops accepting new connections.
2. In-flight requests are completed.
3. Buffered events are flushed to Parquet.

The flush is bounded by `shutdown_timeout_secs` (default 30). If flushing takes longer, a warning is logged and the process exits.

---

## Systemd Service

For non-Docker deployments:

```ini
[Unit]
Description=Mallard Metrics
After=network.target

[Service]
Type=simple
User=mallard
ExecStart=/usr/local/bin/mallard-metrics /etc/mallard-metrics/config.toml
Restart=on-failure
RestartSec=5s
Environment=MALLARD_SECRET=...
Environment=MALLARD_ADMIN_PASSWORD=...

[Install]
WantedBy=multi-user.target
```

```bash
systemctl daemon-reload
systemctl enable --now mallard-metrics
```
