# Health & Metrics Endpoints

These endpoints are publicly accessible (no authentication required) and are designed for monitoring and orchestration systems.

---

## `GET /health`

Simple liveness check. Returns HTTP 200 when the server process is running.

```
HTTP/1.1 200 OK
Content-Type: text/plain

ok
```

Use this with your load balancer or container orchestrator liveness probe.

---

## `GET /health/ready`

Readiness probe. Executes a lightweight DuckDB query to verify the database is operational.

**Success (200):**
```
HTTP/1.1 200 OK
Content-Type: text/plain

ready
```

**Not ready (503):**
```
HTTP/1.1 503 Service Unavailable
Content-Type: text/plain

database not ready
```

Use this as your Kubernetes readiness probe or Docker health check. Do not use it as a liveness probe — a 503 here means the database is temporarily unavailable, not that the process is dead.

---

## `GET /health/detailed`

Detailed system status in JSON. Returns component-level health information.

```json
{
  "status": "ok",
  "version": "0.1.0",
  "buffered_events": 42,
  "auth_configured": true,
  "geoip_loaded": false,
  "behavioral_extension_loaded": true,
  "filter_bots": true,
  "cache_entries": 3,
  "cache_empty": false
}
```

| Field | Type | Description |
|---|---|---|
| `status` | string | Always `"ok"` when the server is running. |
| `version` | string | Binary version from `Cargo.toml`. |
| `buffered_events` | integer | Events in the in-memory buffer, not yet flushed to Parquet. |
| `auth_configured` | boolean | Whether an admin password has been set. |
| `geoip_loaded` | boolean | Whether a MaxMind GeoLite2 database was successfully loaded. |
| `behavioral_extension_loaded` | boolean | Whether the DuckDB `behavioral` extension loaded successfully at startup. |
| `filter_bots` | boolean | Whether bot filtering is active. |
| `cache_entries` | integer | Number of cached query results currently in memory. |
| `cache_empty` | boolean | `true` if the query cache is empty. |

---

## `GET /metrics`

Prometheus-compatible metrics in text exposition format (`text/plain; version=0.0.4`).

If `MALLARD_METRICS_TOKEN` is set, this endpoint requires `Authorization: Bearer <token>`. Returns `401 Unauthorized` without a valid token.

### Gauges

```
# HELP mallard_buffered_events Number of events in the in-memory buffer
# TYPE mallard_buffered_events gauge
mallard_buffered_events 42

# HELP mallard_cache_entries Number of cached query results
# TYPE mallard_cache_entries gauge
mallard_cache_entries 3

# HELP mallard_auth_configured Whether admin password is set
# TYPE mallard_auth_configured gauge
mallard_auth_configured 1

# HELP mallard_geoip_loaded Whether GeoIP database is loaded
# TYPE mallard_geoip_loaded gauge
mallard_geoip_loaded 0

# HELP mallard_filter_bots Whether bot filtering is enabled
# TYPE mallard_filter_bots gauge
mallard_filter_bots 1

# HELP mallard_behavioral_extension Whether behavioral extension is loaded
# TYPE mallard_behavioral_extension gauge
mallard_behavioral_extension 1
```

### Counters

```
# HELP mallard_events_ingested_total Total events ingested via POST /api/event
# TYPE mallard_events_ingested_total counter
mallard_events_ingested_total 158432

# HELP mallard_flush_failures_total Total buffer flush failures
# TYPE mallard_flush_failures_total counter
mallard_flush_failures_total 0

# HELP mallard_rate_limit_rejections_total Total requests rejected by per-site rate limiter
# TYPE mallard_rate_limit_rejections_total counter
mallard_rate_limit_rejections_total 17

# HELP mallard_login_failures_total Total failed login attempts
# TYPE mallard_login_failures_total counter
mallard_login_failures_total 3

# HELP mallard_cache_hits_total Total query cache hits
# TYPE mallard_cache_hits_total counter
mallard_cache_hits_total 9871

# HELP mallard_cache_misses_total Total query cache misses
# TYPE mallard_cache_misses_total counter
mallard_cache_misses_total 1204
```

### Prometheus Scrape Configuration

```yaml
scrape_configs:
  - job_name: mallard_metrics
    static_configs:
      - targets: ['localhost:8000']
    metrics_path: /metrics
    scrape_interval: 30s
    # If MALLARD_METRICS_TOKEN is set:
    authorization:
      credentials: your-metrics-bearer-token
```
