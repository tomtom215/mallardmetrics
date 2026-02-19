# Health & Metrics Endpoints

These endpoints are publicly accessible (no authentication required) and are designed for monitoring systems.

---

## `GET /health`

Simple liveness check.

```
HTTP/1.1 200 OK
Content-Type: text/plain

ok
```

Use this with your load balancer or container orchestrator health check.

---

## `GET /health/detailed`

Detailed system status in JSON.

```json
{
  "status": "ok",
  "version": "0.1.0",
  "buffered_events": 42,
  "auth_configured": true,
  "geoip_loaded": false,
  "filter_bots": true,
  "cache_entries": 3,
  "cache_empty": false
}
```

| Field | Type | Description |
|---|---|---|
| `status` | string | Always `"ok"` when the server is running. |
| `version` | string | Binary version from `Cargo.toml`. |
| `buffered_events` | integer | Events in the in-memory buffer not yet flushed to Parquet. |
| `auth_configured` | boolean | Whether an admin password has been set. |
| `geoip_loaded` | boolean | Whether a MaxMind GeoLite2 database was successfully loaded. |
| `filter_bots` | boolean | Whether bot filtering is active. |
| `cache_entries` | integer | Number of cached query results currently in memory. |
| `cache_empty` | boolean | `true` if query cache is empty. |

---

## `GET /metrics`

Prometheus-compatible metrics in text exposition format (`text/plain; version=0.0.4`).

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
```

### Prometheus Scrape Configuration

```yaml
scrape_configs:
  - job_name: mallard_metrics
    static_configs:
      - targets: ['localhost:8000']
    metrics_path: /metrics
```
