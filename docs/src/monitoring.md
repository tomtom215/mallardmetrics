# Monitoring

## Health Checks

Three health endpoints are available without authentication:

### `GET /health`

Returns `ok` with HTTP 200 when the server is running. Use this for:
- Load balancer health checks.
- Container orchestrator liveness probes.

```yaml
# Kubernetes liveness probe
livenessProbe:
  httpGet:
    path: /health
    port: 8000
  initialDelaySeconds: 5
  periodSeconds: 10
```

### `GET /health/ready`

Executes a lightweight DuckDB query (`SELECT 1 FROM events_all LIMIT 0`) to verify the database is operational. Returns:

- `200 OK` — database is ready and accepting queries.
- `503 Service Unavailable` — database is not ready (use this as a readiness probe, not liveness).

```yaml
# Kubernetes readiness probe
readinessProbe:
  httpGet:
    path: /health/ready
    port: 8000
  initialDelaySeconds: 10
  periodSeconds: 15
  failureThreshold: 3
```

### `GET /health/detailed`

Returns a JSON object with component-level status. See [Health & Metrics API](api-reference/health.md) for the full schema.

---

## Prometheus Metrics

`GET /metrics` returns Prometheus text format metrics (`text/plain; version=0.0.4`).

If `MALLARD_METRICS_TOKEN` is set, this endpoint requires `Authorization: Bearer <token>`.

### Gauges

| Metric | Type | Description |
|---|---|---|
| `mallard_buffered_events` | gauge | Events in memory, not yet flushed to Parquet |
| `mallard_cache_entries` | gauge | Cached query results in memory |
| `mallard_auth_configured` | gauge | `1` if admin password is set, `0` otherwise |
| `mallard_geoip_loaded` | gauge | `1` if GeoIP database loaded successfully |
| `mallard_filter_bots` | gauge | `1` if bot filtering is active |
| `mallard_behavioral_extension` | gauge | `1` if behavioral extension loaded, `0` otherwise |

### Counters

| Metric | Type | Description |
|---|---|---|
| `mallard_events_ingested_total` | counter | Total events accepted through `POST /api/event` |
| `mallard_flush_failures_total` | counter | Total buffer flush failures |
| `mallard_rate_limit_rejections_total` | counter | Total requests rejected by the per-site rate limiter |
| `mallard_login_failures_total` | counter | Total failed login attempts |
| `mallard_cache_hits_total` | counter | Total query cache hits |
| `mallard_cache_misses_total` | counter | Total query cache misses |

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
      credentials: your-metrics-token
```

### Example Output

```
# HELP mallard_buffered_events Number of events in the in-memory buffer
# TYPE mallard_buffered_events gauge
mallard_buffered_events 42

# HELP mallard_cache_entries Number of cached query results
# TYPE mallard_cache_entries gauge
mallard_cache_entries 3

# HELP mallard_behavioral_extension Whether behavioral extension is loaded
# TYPE mallard_behavioral_extension gauge
mallard_behavioral_extension 1

# HELP mallard_events_ingested_total Total events ingested
# TYPE mallard_events_ingested_total counter
mallard_events_ingested_total 158432

# HELP mallard_cache_hits_total Total query cache hits
# TYPE mallard_cache_hits_total counter
mallard_cache_hits_total 9871

# HELP mallard_cache_misses_total Total query cache misses
# TYPE mallard_cache_misses_total counter
mallard_cache_misses_total 1204
```

### Grafana Dashboard

A minimal Grafana panel configuration for key metrics:

```json
{
  "panels": [
    {
      "title": "Ingestion Rate",
      "targets": [{"expr": "rate(mallard_events_ingested_total[5m])"}]
    },
    {
      "title": "Buffered Events",
      "targets": [{"expr": "mallard_buffered_events"}]
    },
    {
      "title": "Cache Hit Rate",
      "targets": [{"expr": "rate(mallard_cache_hits_total[5m]) / (rate(mallard_cache_hits_total[5m]) + rate(mallard_cache_misses_total[5m]))"}]
    },
    {
      "title": "Rate Limit Rejections",
      "targets": [{"expr": "rate(mallard_rate_limit_rejections_total[5m])"}]
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
{"timestamp":"2024-01-15T10:00:00.123Z","level":"INFO","fields":{"message":"Flushed events to Parquet","count":42},"target":"mallard_metrics::ingest::buffer","request_id":"a3f2c1d8-..."}
```

Every log line emitted during a request carries a `request_id` field matching the `X-Request-ID` response header, enabling end-to-end log correlation.

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
| High flush failures | `increase(mallard_flush_failures_total[5m]) > 0` | Warning |
| Auth not configured | `mallard_auth_configured == 0` | Warning |
| High rate limit rejections | `rate(mallard_rate_limit_rejections_total[5m]) > 10` | Info |
| Low cache hit rate | `(cache_hits / (cache_hits + cache_misses)) < 0.5` | Info |
| GeoIP not loaded | `mallard_geoip_loaded == 0` | Info |
| Behavioral extension missing | `mallard_behavioral_extension == 0` | Info |
