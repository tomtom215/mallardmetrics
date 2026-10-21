# Privacy and GDPR

A task-oriented guide: what is stored, what a visitor ID is, what the privacy
settings change, how erasure behaves, and what the bundled reverse proxy logs.
It describes behaviour, not legal advice; the legal analysis and operator
obligations are in
[PRIVACY.md](https://github.com/tomtom215/mallardmetrics/blob/main/PRIVACY.md).

## What is stored

Per event, in the `events` table and the Parquet files
([schema](data-management.md#schema)):

| Stored | Notes |
|---|---|
| `site_id`, `hostname` | The `d` value you configured in the tracker. |
| `timestamp` | UTC with sub-second precision (the hour with `round_timestamps`). |
| `visitor_id` | A keyed hash, below. |
| `event_name`, `pathname` | The path only. The query string and fragment are dropped, so a path like `/users/jane` **is** stored: keep personal data out of URL paths. |
| `referrer`, `referrer_source` | The full referrer URL (the query and fragment too, unless `strip_referrer_query`), and a source name derived from its host. |
| `utm_*` | Parsed from the page URL before it is discarded. |
| `browser`, `os` and their versions | Parsed from the `User-Agent` and low-entropy Client Hints. The raw User-Agent string is **not** stored. |
| `device_type`, `screen_size` | The viewport width (not the physical screen) and a class derived from it. |
| `country_code`, `region`, `city` | Only with a GeoIP database, limited by `geoip_precision`. |
| `props`, `revenue_*` | Whatever your code sends. Custom properties are free text: you decide what goes in. |

**Never stored:** the IP address (it is hashed, used for GeoIP and the in-memory
rate limiters, then dropped; it is not in the database, the Parquet files or the
application log), cookies, the raw User-Agent, any browser storage value.

The tracking script sets no cookies and writes nothing to browser storage. It
**reads** exactly one `localStorage` key, `mallard_ignore`: setting it to `true`
opts that browser out (for example your own). It sends nothing else about the
browser than the fields above.

## What a visitor ID is

```
salt       = HMAC-SHA256(key = MALLARD_SECRET, msg = "mallard-metrics/salt/v2" | period)
visitor_id = HMAC-SHA256(key = salt,           msg = site_id | IP | User-Agent)
```

`period` is the number of days since 1970-01-01 divided by
`visitor_salt_rotation_days` (rounded down), so periods are aligned to UTC
midnight and identical across instances that share a secret.

- It is **pseudonymous personal data** (GDPR Recital 26): whoever holds the secret
  and a candidate IP and User-Agent can recompute it, so the secret is the
  protection.
- It is **scoped per site**: the same person on two sites of one instance has
  two unrelated IDs.
- It is **stable within a period** and unlinkable across periods.
- Two people behind one NAT with the same browser share an ID; one person on two
  browsers has two. Unique visitors are an estimate.

## Salt rotation

`visitor_salt_rotation_days` (default `1`) decides how long an ID lives.

| Setting | Effect |
|---|---|
| `1` (default) | IDs change every UTC midnight. Most private. Unique visitors over a range count visitor-days; weekly retention reads zero past week 0; a visit across midnight is two visits. |
| `7`, `30` | Cross-day analysis works for that long, at the cost of a longer-lived pseudonym. Record the decision in your privacy notice. Retention over *N* weeks needs `(N - 1) * 7`. |
| Changing the value | Period boundaries move, so IDs change once at the switch; old and new data are not linkable. |

**Changing `MALLARD_SECRET`** re-identifies every visitor from that moment: the
same person gets a new ID, unique-visitor and retention figures across the change
are not comparable, and funnels in progress at the moment of the change break.
Old stored IDs remain valid hashes but can no longer be recomputed, which is also
what makes a deliberate secret rotation a way to cut linkability to old data. The
generated `data_dir/.secret` is persisted, so a restart does not rotate it;
losing it (or restoring a backup without it) does.

## GDPR mode

`MALLARD_GDPR_MODE=true` is a preset that forces on:

| Flag | Effect |
|---|---|
| `strip_referrer_query` | Referrers stored without `?query` and `#fragment`. |
| `round_timestamps` | Time rounded down to the hour. |
| `suppress_browser_version`, `suppress_os_version` | Names only. |
| `suppress_screen_size` | No width and no device type. |
| `geoip_precision` | At most `country`. |

It deliberately does **not** enable `suppress_visitor_id`, which removes unique
visitors entirely.

**Analytical cost (verified).** Because time is rounded to the hour, events
inside an hour cannot be ordered, so sessions, funnels, sequences, flow and the
entry/exit page reports answer `503` (`behavioral_unavailable`), and the session
fields on `/api/stats/main` are `null`. Counts, breakdowns, time series, goals,
revenue, properties and retention still work. If you need sessions, enable the
individual flags instead and leave `round_timestamps` off. The server warns at
startup when `gdpr_mode` is on and `retention_days` is 0.

## Erasure

`DELETE /api/gdpr/erase?site_id=...&start_date=...&end_date=...` (admin
authentication, see the [API reference](api-reference/stats.md#delete-apigdprerase)).

- **Granularity is site plus inclusive date range.** There is no per-visitor
  erasure: stored IDs are hashes that cannot be mapped to a person without the
  original IP and User-Agent, so an individual's rows cannot be selected. Say so
  in your privacy notice, and use `retention_days` for routine minimisation.
- **Order.** Buffered events are flushed first so they cannot reappear after the
  erasure; then the hot table and the Parquet partitions are cleared under the
  exclusive tier lock (no report sees a half-erased state), the query view is
  refreshed and that site's cached results are dropped.
- **What gets removed.** Each `date=` directory is deleted with everything in it,
  including `.pending`, `.del` and temporary files that can hold the same rows.
- **Success.** `200` with `"status": "erased"`, `db_records_deleted` (hot-table
  rows only, usually 0 or small) and `parquet_partitions_deleted`.
- **Partial failure.** If a directory cannot be removed, the answer is `500` with
  `"status": "partial"` and `failed_dates`. Those dates are **still on disk**;
  the other dates were erased. Fix the cause and repeat the request, which is
  idempotent. Treat anything other than `200 erased` as not done.
- **Backups are not touched.** Snapshots made before the request still contain the
  data. Expire them on your own schedule and record that in your procedure; if a
  restore is ever needed, repeat the erasure afterwards.

## What the bundled Caddy logs

`deploy/Caddyfile` removes everything that identifies a visitor from every
logger, using a shared `privacy_log_format`:

| Logged | Not logged |
|---|---|
| method, host, path, status, size, duration, timestamp | remote IP and port, client IP, **all request headers** (so no User-Agent, Cookie, Referer), TLS details, user ID, response headers |
| the query string **replaced by** `?[redacted]` | the query string itself (the pixel tracker carries page URLs in it) |

Access logs go to stdout and the runtime/error log to stderr (also scrubbed, at
`WARN`); Docker's `json-file` driver rotates them by size (20 MB x 5 for Caddy,
50 MB x 7 for Mallard). There is no log file on the data volume and no
time-based retention to forget. Delete the `log` block to log no requests at all.

This holds for the **bundled** Caddy only. Another proxy (nginx, a CDN, a load
balancer) logs IPs by default; configure it the same way or document it as a
processor of IP addresses. The Mallard process itself logs no visitor IP.

## Checklist

- [ ] Decide `visitor_salt_rotation_days` and record why.
- [ ] Decide between `gdpr_mode` (no sessions) and individual flags.
- [ ] Set `retention_days` to your retention period.
- [ ] Keep personal data out of URL paths and custom properties.
- [ ] Set `MALLARD_SECRET` explicitly, store it separately from backups.
- [ ] Describe erasure by site and date, and backup retention, in the notice.
- [ ] Confirm what any proxy or CDN in front of Mallard logs.
