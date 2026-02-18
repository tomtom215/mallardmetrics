# Monitoring

## Health Checks

Two health endpoints are available without authentication:

### `GET /health`

Returns `ok` with HTTP 200 when the server is running. Use this for:
- Load balancer health checks.
- Container orchestrator readiness/liveness probes.

```yaml
# Kubernetes liveness probe
livenessProbe:
  httpGet:
    path: /health
    port: 8000
  initialDelaySeconds: 5
  periodSeconds: 10
```

### `GET /health/detailed`

Returns a JSON object with component-level status. See [Health & Metrics API](api-reference/health.md) for the full schema.

---

## Prometheus Metrics

`GET /metrics` returns Prometheus text format metrics. Scraped metrics:

| Metric | Type | Description |
|---|---|---|
| `mallard_buffered_events` | gauge | Events in memory, not yet flushed to Parquet |
| `mallard_cache_entries` | gauge | Cached query results in memory |
| `mallard_auth_configured` | gauge | `1` if admin password is set, `0` otherwise |
| `mallard_geoip_loaded` | gauge | `1` if GeoIP database loaded successfully |
| `mallard_filter_bots` | gauge | `1` if bot filtering is active |

### Prometheus Configuration

```yaml
scrape_configs:
  - job_name: mallard_metrics
    static_configs:
      - targets: ['localhost:8000']
    metrics_path: /metrics
    scrape_interval: 30s
```

### Grafana Dashboard

Import a simple Grafana dashboard to visualize the above metrics. A minimal panel configuration:

```json
{
  "panels": [
    {
      "title": "Buffered Events",
      "targets": [{"expr": "mallard_buffered_events"}]
    },
    {
      "title": "Cache Entries",
      "targets": [{"expr": "mallard_cache_entries"}]
    }
  ]
}
```

---

## Structured Logging

Mallard Metrics uses [`tracing`](https://crates.io/crates/tracing) for structured logging. Two formats are supported:

### Text (default)

Human-readable output with timestamps, log levels, and structured fields:

```
2024-01-15T10:00:00.123Z  INFO mallard_metrics: Starting Mallard Metrics host="0.0.0.0" port=8000
2024-01-15T10:00:00.456Z  INFO mallard_metrics: Behavioral extension loaded
2024-01-15T10:00:00.457Z  INFO mallard_metrics: Listening addr="0.0.0.0:8000"
```

### JSON

Set `MALLARD_LOG_FORMAT=json` for machine-parseable output compatible with log aggregators (Loki, Elasticsearch, Splunk):

```json
{"timestamp":"2024-01-15T10:00:00.123Z","level":"INFO","fields":{"message":"Flushed events to Parquet","count":42},"target":"mallard_metrics::ingest::buffer"}
```

### Log Level Control

Use the `RUST_LOG` environment variable (standard `tracing-subscriber` env-filter syntax):

```bash
RUST_LOG=mallard_metrics=debug,tower_http=info
```

Default: `mallard_metrics=info,tower_http=info`

---

## Alerting Recommendations

| Alert | Condition | Severity |
|---|---|---|
| Server down | `up{job="mallard_metrics"} == 0` | Critical |
| Large event buffer | `mallard_buffered_events > 5000` | Warning |
| Auth not configured | `mallard_auth_configured == 0` | Warning |
| GeoIP not loaded | `mallard_geoip_loaded == 0` | Info |
