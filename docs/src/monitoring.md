# Monitoring

## Health checks

Three unauthenticated endpoints (full schemas in
[Health & Metrics](api-reference/health.md)):

| Endpoint | Use it for | Notes |
|---|---|---|
| `GET /health` | Liveness | `200 ok`. Does not touch the database. |
| `GET /health/ready` | Readiness, container health | `200 ready`, or `503 database not ready`. |
| `GET /health/detailed` | Humans and synthetic checks | `503` with `"status":"degraded"` when the cold tier is unreadable. |

```yaml
# Kubernetes
livenessProbe:
  httpGet: { path: /health, port: 8000 }
  initialDelaySeconds: 5
  periodSeconds: 10
readinessProbe:
  httpGet: { path: /health/ready, port: 8000 }
  initialDelaySeconds: 10
  periodSeconds: 15
  failureThreshold: 3
```

Give the first start enough time: if the `behavioral` extension has to be
downloaded, the server does not bind its port until that attempt has finished.
The shipped images declare `HEALTHCHECK ... --start-period=20s` for this reason.

For an external uptime monitor, alert on `/health/detailed` returning a non-200
status: it is the only check that notices a cold tier that has fallen out of the
query view while ingestion keeps succeeding.

---

## Prometheus metrics

`GET /metrics`, `text/plain; version=0.0.4`, protected by
`Authorization: Bearer $MALLARD_METRICS_TOKEN` when that variable is set.
The tables below were generated from a real response; names, types and `HELP`
text are exact.

### Metric reference

| Metric | Type | `HELP` text |
|---|---|---|
| `mallard_build_info{version,behavioral_version}` | gauge (always 1) | Version of this binary and of the loaded behavioral extension (`behavioral_version="none"` when not loaded) |
| `mallard_buffered_events` | gauge | Events in the in-memory buffer |
| `mallard_cache_entries` | gauge | Cached query results |
| `mallard_active_sessions` | gauge | Live dashboard sessions |
| `mallard_read_connections` | gauge | DuckDB connections serving analytics queries |
| `mallard_auth_configured` | gauge | Whether an admin password is set |
| `mallard_geoip_loaded` | gauge | Whether a GeoIP database is loaded |
| `mallard_behavioral_extension` | gauge | Whether the DuckDB behavioral extension is loaded |
| `mallard_filter_bots` | gauge | Whether bot filtering is enabled |
| `mallard_events_ingested_total` | counter | Events buffered since startup |
| `mallard_events_dropped_total` | counter | Events discarded because the buffer was at capacity |
| `mallard_flush_failures_total` | counter | Parquet flush failures since startup (periodic flush task) |
| `mallard_buffer_flush_failures_total` | counter | Flushes of the in-memory buffer that failed after retries inside push (threshold-triggered flushes) |
| `mallard_rate_limiter_evictions_total` | counter | Rate-limiter buckets dropped early because the key table was full (sum of the site and IP limiters) |
| `mallard_process_start_time_seconds` | gauge | Unix time at which the process started |
| `mallard_last_flush_success_timestamp_seconds` | gauge | Unix time of the last flush that completed without error (0 before the first) |
| `mallard_rate_limit_rejections_total` | counter | Ingest requests rejected by a rate limiter (per-site and per-IP, POST and pixel) |
| `mallard_login_failures_total` | counter | Failed login attempts since startup |
| `mallard_cache_hits_total` | counter | Query cache hits since startup |
| `mallard_cache_misses_total` | counter | Query cache misses since startup |
| `mallard_cache_evictions_total` | counter | Cache entries evicted to stay within the entry cap |
| `mallard_notifications_sent_total{kind}` | counter | Webhook notifications the receiver accepted, by kind (`alert`, `resolved`, `report`) |
| `mallard_notifications_failed_total{kind}` | counter | Webhook notifications abandoned after their retries, by kind |
| `mallard_notifications_dropped_total{kind}` | counter | Webhook notifications discarded unsent (queue full or shutdown), by kind |
| `mallard_notification_last_success_timestamp_seconds` | gauge | Unix time of the last webhook delivery the receiver accepted (0 before the first) |
| `mallard_notification_queue_depth` | gauge | Webhook notifications waiting to be delivered |
| `mallard_alerts_active` | gauge | Alerts currently firing, as seen by the alert evaluator |
| `mallard_http_requests_total{status}` | counter | HTTP responses served, by status class (`1xx`..`5xx`) |
| `mallard_http_request_duration_seconds` | histogram | Time to produce a response. Buckets (`le`): 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, `+Inf`, plus `_sum` and `_count` |

Notes:

- `mallard_process_start_time_seconds` and
  `mallard_last_flush_success_timestamp_seconds` are gauges even though they sit
  beside the counters in the output.
- A flush that finds nothing to write still counts as a success: the timestamp
  says "the flush loop is alive", not "traffic arrived".
- Counts are by status class, not by route: `/{*path}` matches anything a
  crawler asks for, so a per-route label would have unbounded cardinality.
- The histogram is recorded by the outermost middleware, so it sees the status
  the client received (a request cut off by the timeout is a `408`).
- All counters reset on restart; `rate()` and `increase()` handle that.

### Useful expressions

```promql
# 5xx ratio above 1% for 5 minutes
sum(rate(mallard_http_requests_total{status="5xx"}[5m]))
  / sum(rate(mallard_http_requests_total[5m])) > 0.01

# 95th percentile latency above 2 s
histogram_quantile(0.95, sum(rate(mallard_http_request_duration_seconds_bucket[5m])) by (le)) > 2

# The flush loop has stopped (needs one flush interval of grace; default 60 s)
time() - mallard_last_flush_success_timestamp_seconds > 300

# Any dropped event means data loss
increase(mallard_events_dropped_total[15m]) > 0

# The extension version changed (after an upgrade)
changes(mallard_build_info[1d]) > 0
```

`mallard_events_dropped_total` is the one to page on. It only moves when flushes
fail (full disk, bad permissions) long enough for the buffer to reach
`max_buffered_events`.

### Ready-made rules and dashboard

The repository ships `deploy/monitoring/`:

| File | Content |
|---|---|
| `prometheus-scrape.example.yml` | Scrape job with the bearer token read from a file |
| `prometheus-rules.yml` | Alerts: `MallardDown`, `MallardAdminNotConfigured`, `MallardBehavioralExtensionMissing`, `MallardFlushFailing`, `MallardEventsDropped`, `MallardBufferBacklog`, `MallardNoEventsReceived`, `MallardServerErrors`, `MallardSlowResponses`, `MallardLoginFailuresSpike`, `MallardRateLimiting`, `MallardNotificationsFailing`, `MallardNotificationsDropped` |
| `prometheus-rules.test.yml` | `promtool test rules` unit tests for the alerts |
| `grafana-dashboard.json` | Dashboard: up, extension, admin password, GeoIP, ingest rate, buffer, flush failures, HTTP classes, latency, cache, failed logins, sessions |

Validate with `promtool check rules deploy/monitoring/prometheus-rules.yml`.
(The header note in `prometheus-rules.yml` that lists "last successful flush"
and "build/behavioral version" as not yet alertable predates the metrics above.)
Disk space is not exported by Mallard; use `node_exporter`
(`node_filesystem_avail_bytes` on the data volume).

---

## Logs

Mallard uses [`tracing`](https://crates.io/crates/tracing) and writes to
**stderr**. Two formats:

**Text (default):**

```
2026-10-21T00:18:40.131642Z  INFO mallard_metrics: Starting Mallard Metrics version="0.2.0" host=127.0.0.1 port=18741 data_dir=/data
2026-10-21T00:18:40.889017Z  INFO mallard_metrics: Behavioral extension loaded version="0.10.0"
2026-10-21T00:18:41.496418Z  INFO mallard_metrics: Listening addr=127.0.0.1:18741
```

Text output is coloured even when redirected to a file (observed); set `NO_COLOR=1`
to remove the escape codes (verified).

**JSON** (`MALLARD_LOG_FORMAT=json`), one object per line:

```json
{"timestamp":"2026-10-21T00:26:56.314757Z","level":"INFO","fields":{"message":"Behavioral extension loaded","version":"0.10.0"},"target":"mallard_metrics"}
```

Lines emitted while serving a request carry a `request_id` matching the
`X-Request-ID` response header.

**Level:** `RUST_LOG`, standard `tracing-subscriber` filter syntax. Default
`mallard_metrics=info,tower_http=info`.

See [Troubleshooting](troubleshooting.md#reading-the-logs) for which messages
matter.

---

## Webhook alerts and reports

Prometheus is the right tool when you run one. When you do not, Mallard can push
its own alerts (flush stalled, events failing to reach disk, extension missing,
cold tier degraded, rate-limiter evictions spiking, abnormal traffic) and daily
or weekly reports to a signed webhook, including Slack and Discord. It is off by
default and has its own page: [Webhook Alerts and Reports](alerts-and-reports.md).
The notifier's own health is on `/metrics` (the `mallard_notification*` family
above) and `MallardNotificationsFailing` fires when deliveries are abandoned.

---

## Alerting recommendations

| Alert | Condition | Severity |
|---|---|---|
| Server down | `up == 0` for 5 min | Critical |
| Admin password not set | `mallard_auth_configured == 0` | Critical on a networked host |
| Data loss | `increase(mallard_events_dropped_total[15m]) > 0` | Critical |
| Flush failing | `increase(mallard_flush_failures_total[15m]) > 0` or the stalled-flush expression above | Warning |
| `/health/detailed` degraded | External check, non-200 | Critical |
| Behavioral extension missing | `mallard_behavioral_extension == 0` for 10 min | Warning |
| 5xx rate, slow responses | expressions above | Warning |
| Login failures spike | `increase(mallard_login_failures_total[10m]) > 20` | Warning |
| Rate limiting | `increase(mallard_rate_limit_rejections_total[10m]) > 100` | Info |
| Disk filling | `node_exporter` on the data volume | Warning |
