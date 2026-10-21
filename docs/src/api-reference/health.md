# Health & Metrics Endpoints

These endpoints need no login. They are meant for load balancers, orchestrators
and Prometheus. All examples below are real output from a 0.2.0 binary.

---

## `GET /health`

Liveness. `200` with the plain-text body `ok` whenever the process answers.

```
HTTP/1.1 200 OK
content-type: text/plain; charset=utf-8

ok
```

Use it as a liveness probe. It does not touch the database.

---

## `GET /health/ready`

Readiness. Runs `SELECT 1 FROM events_all LIMIT 0` on a read connection.

| Status | Body | Meaning |
|---|---|---|
| 200 | `ready` | DuckDB is reachable and `events_all` is queryable. |
| 503 | `database not ready` | The query failed. |

Use it as a readiness probe, not liveness: a `503` means "hold traffic", and
restarting rarely helps. The container image's `HEALTHCHECK`
(`mallard-metrics --healthcheck`) probes this path on the loopback address.

---

## `GET /health/detailed`

Component status as JSON. Unauthenticated, so it deliberately omits whether an
admin password is set, how many sessions are live, and whether proxy headers are
trusted (those are on `/metrics`, which can be token-protected).

```json
{
  "behavioral_extension_loaded": true,
  "behavioral_version": "0.10.0",
  "buffer_empty": true,
  "buffered_events": 0,
  "cache_empty": true,
  "cache_entries": 0,
  "cold_tier": "healthy",
  "dropped_events": 0,
  "filter_bots": true,
  "geoip_loaded": false,
  "read_connections": 4,
  "session_window": "30 minutes",
  "status": "ok",
  "version": "0.2.0",
  "visitor_salt_rotation_days": 1
}
```

| Field | Type | Description |
|---|---|---|
| `status` | string | `"ok"`, or `"degraded"` (HTTP **503**) when `cold_tier` is `degraded`. |
| `cold_tier` | string | `healthy`: the Parquet files are being read. `empty`: no Parquet files exist yet (normal on a fresh install). `degraded`: Parquet files exist but `events_all` does not read them, or reads a pattern that matches nothing, so reports silently omit flushed data. |
| `version` | string | Binary version. |
| `buffered_events` / `buffer_empty` | integer / boolean | Events in memory, not yet flushed (and not yet queryable). |
| `dropped_events` | integer | Events discarded because the buffer hit `max_buffered_events`. Non-zero means data loss. |
| `geoip_loaded` | boolean | A GeoIP database was opened. |
| `behavioral_extension_loaded` | boolean | The extension is loaded, at version 0.10.0 or newer. Normally decided at startup; if the first install failed, a background task retries every 60 s and this can turn `true` later. |
| `behavioral_version` | string or `null` | Version the extension reports. |
| `read_connections` | integer | Size of the read pool (1 means reads share the writer connection). |
| `filter_bots` | boolean | Bot filtering is on. |
| `session_window` | string | The session inactivity window as a DuckDB interval. |
| `visitor_salt_rotation_days` | integer | Salt period; see [Privacy and GDPR](../privacy-gdpr.md). |
| `cache_entries` / `cache_empty` | integer / boolean | Cached query results. |

Removed since 0.1.x: `auth_configured`, `active_sessions` (use the
`mallard_auth_configured` and `mallard_active_sessions` gauges).

**A `503` here with `"status":"degraded"` is an outage that looks healthy**:
ingestion and `/health` still succeed. See
[Troubleshooting](../troubleshooting.md#degraded-cold-tier).

---

## `GET /metrics`

Prometheus text exposition, `Content-Type: text/plain; version=0.0.4`.

If `MALLARD_METRICS_TOKEN` is set the request needs
`Authorization: Bearer <token>` (compared in constant time). Without it the
answer is `401` with the plain-text body `Unauthorized`.

```bash
curl -H "Authorization: Bearer $MALLARD_METRICS_TOKEN" http://localhost:8000/metrics
```

The complete list of 23 metric families, with types and the exact `HELP` text, is on the
[Monitoring](../monitoring.md#metric-reference) page. Highlights:

```
# HELP mallard_build_info Version of this binary and of the loaded behavioral extension
# TYPE mallard_build_info gauge
mallard_build_info{version="0.2.0",behavioral_version="0.10.0"} 1
# HELP mallard_events_dropped_total Events discarded because the buffer was at capacity
# TYPE mallard_events_dropped_total counter
mallard_events_dropped_total 0
# HELP mallard_last_flush_success_timestamp_seconds Unix time of the last flush that completed without error (0 before the first)
# TYPE mallard_last_flush_success_timestamp_seconds gauge
mallard_last_flush_success_timestamp_seconds 1792541951
# HELP mallard_http_requests_total HTTP responses served, by status class
# TYPE mallard_http_requests_total counter
mallard_http_requests_total{status="2xx"} 10
```

### Prometheus scrape configuration

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

A ready-made scrape example, alert rules (with a unit-test file) and a Grafana
dashboard are in `deploy/monitoring/` in the repository.

`robots.txt` disallows `/api/`, `/health` and `/metrics`.
