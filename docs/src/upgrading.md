# Upgrading

This page covers moving from **0.2.0** to the next release. The database schema
version is unchanged (`1`), so no data migration runs and existing Parquet files
are read as they are; the changes are in startup checks, report definitions, the
API and the image. Read the lists below before you upgrade: several of them stop
the server from starting or change a number on your dashboard.

Each item says how it was verified: **run** means observed against the binary
built from this tree (the 0.2.0 line, before release of the next version);
**code** means read from the source. Statements about what 0.2.0 did before come
from the changelog and the earlier documentation, not from running 0.2.0.

## Before you start

1. Take a [backup](backup-restore.md) (stop, copy, start). The schema version
   does not change, so an older binary can open the data again, but a backup is
   the only real undo for the visitor-ID and secret decisions below.
2. Record a few report numbers for a **complete past period with explicit dates**
   (for example `start_date=2026-10-01&end_date=2026-10-07`), so you can compare
   after the upgrade (see [comparing numbers](#definitions-changed-compare-with-care)).
3. Read the startup log of the new version before sending traffic to it: every
   rejected setting is reported there.

## Breaking: the server may refuse to start

| Change | You will see | Fix | Verified |
|---|---|---|---|
| `MALLARD_SECRET` shorter than **16 characters** is refused. | `MALLARD_SECRET is shorter than 16 characters. Use a random value, e.g. openssl rand -hex 32.`, exit 1 | Set a longer value. **Changing the secret re-identifies every visitor**, so you must choose between keeping the short secret on the old version and accepting new visitor IDs (see [Privacy and GDPR](privacy-gdpr.md#salt-rotation)). The auto-generated `.secret` is a UUID and passes. | run |
| `MALLARD_ADMIN_PASSWORD` must pass the setup policy: 12+ characters, not an obvious value. | `MALLARD_ADMIN_PASSWORD rejected: Password must be at least 12 characters` | Use a longer password. | run |
| An unreadable `data_dir/.secret` is refused instead of replaced. | `Cannot read ...; refusing to replace the visitor-ID secret` | Fix the permissions (the container user is UID 65532). | code |
| `login_lockout_secs = 0` is an error. | `login_lockout_secs must be > 0` | Use a positive value, or `max_login_attempts = 0` to turn lockout off. | run |
| `trusted_proxy_hops` outside 1 to 8 while `trust_proxy_headers` is on. | `trusted_proxy_hops must be between 1 and 8 ...` | Set the number of proxies you own. | run |
| A data directory path containing `*`, `?`, `[` or `]`. | `the data directory ... contains the glob character ...` | Move the data. | run |
| `session_ttl_secs = 0`, `flush_*` = 0, `session_window_minutes = 0`, etc. | `... must be > 0` | Use positive values. | run (`session_ttl_secs`), code |
| Unknown TOML key. | `unknown field ..., expected one of ...` | Fix the typo. (Already true in 0.2.0.) | run |
| An older `behavioral` build already cached on the data volume. | `behavioral extension 0.9.1 is older than the required 0.10.0; delete the cached behavioral.duckdb_extension ...` in the log, then every behavioral endpoint answers `503` (the server still starts). | Clean the extension cache, below. | code |

### Cleaning the extension cache

`INSTALL` does nothing when a file is already present, so a data volume that
cached an older build keeps serving it, and the new minimum (0.10.0) is
refused. The cache lives under the extension directory
(`data_dir/extensions`, `/data/extensions` in the image) as
`v<duckdb version>/<platform>/behavioral.duckdb_extension` plus a
`behavioral.duckdb_extension.info`. Delete both for the platform, then restart
(or re-run [`--install-extension`](troubleshooting.md#running-the-extension-install-step-offline)
on an isolated host):

```bash
docker compose stop mallard
sudo rm -f /srv/mallard/data/extensions/v*/*/behavioral.duckdb_extension*
docker compose start mallard
curl -s http://localhost:8000/health/detailed | grep behavioral_version   # "0.10.0" or newer
```

The new release bundles DuckDB 1.5.6, so a volume that last installed under
another DuckDB version has a different `v...` directory and downloads fresh.

## Breaking: the image and the platform

- **There is no musl build and no `FROM scratch` image any more.** The
  `behavioral` extension is not published for `linux_amd64_musl` or
  `linux_arm64_musl` (the community repository answers 404; `linux_amd64`,
  `linux_arm64`, `osx_amd64` and `osx_arm64` exist), so a musl binary ingested
  perfectly and answered `503` on every funnel, retention, session, sequence and
  flow request. The image is a glibc binary on distroless `cc-debian12`, running as
  **UID/GID 65532**; release archives are `*-unknown-linux-gnu` and Apple targets.
  If you built your own static musl binary, rebuild for a `-gnu` target.
- **Data ownership.** The container user is 65532. A bind-mounted data directory
  created for an older image may be owned by another user: run
  `sudo chown -R 65532:65532 <data dir>` before the first start. (`setup.sh` does
  this for new installs; a *new* named volume inherits the ownership from the
  image, an existing one keeps what it has.)
- **No shell in the image** (as before), so `docker exec` cannot run commands in it;
  use `docker run --rm ... --install-extension` style invocations instead.
- **Build from source** needs Rust 1.98.0 (pinned in `rust-toolchain.toml`).
- **Compose.** `stop_grace_period: 45s` replaces Docker's default 10 s, because
  the shutdown flush can take up to the shutdown timeout. If you keep your own
  compose file, add it, or buffered events can be lost on `docker stop`.

## Breaking: behaviour behind a reverse proxy

`MALLARD_TRUSTED_PROXY_HOPS` (default 1) makes the client address entry *N from the
right* of `X-Forwarded-For`, not the leftmost entry. With a single proxy
(Caddy, or nginx with `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`)
nothing changes. With a CDN in front of the proxy set it to `2`. The leftmost
entry is client-supplied and is no longer believed. See
[Configuration](configuration.md#trust_proxy_headers-and-trusted_proxy_hops).
The production compose file already sets
`MALLARD_TRUST_PROXY_HEADERS=true` and `MALLARD_TRUSTED_PROXY_HOPS=1`.

## Breaking: API changes

| Change | Detail | Verified |
|---|---|---|
| Error bodies gain `code`. | `{"error": "...", "code": "bad_request"}`. Existing `error` handling keeps working. | run |
| Query timeout is `504`, not `400`. | `code: query_timeout`. Retry with a shorter range. | code |
| `429` always has `Retry-After`. | Ingest limit, query queue and lockout. | run |
| Queries queue for up to 10 s before `429`, and run for at most 20 s. | Fewer spurious `429`s on dashboard load. | code |
| GDPR erase can answer `500` with `"status": "partial"` and `failed_dates`. | Treat anything but `200 erased` as not done, and retry. | code |
| `/health/detailed` lost `auth_configured`, `active_sessions`, `trust_proxy_headers`; gained `cold_tier`, `behavioral_version`, `buffer_empty`, `dropped_events`, `read_connections`, `session_window`, `visitor_salt_rotation_days`; and it answers **503** when `cold_tier` is `degraded`. | A probe that treated any non-200 as "restart" will now restart a degraded instance: use `/health/ready` for readiness. | run |
| `HEAD /api/event` answers like the pixel and records nothing. | | run |
| `GET /api/event` always answers the GIF. | | run |
| Entry and exit page rows carry `sessions`. | `pageviews`/`events` are totals over those sessions. | run |
| Retention cohorts carry `complete_weeks` and `partial_first_week`. | Additive. | run |
| `period=Nd` is N days including today. | `7d` was eight days. | code, run |
| A lone `start_date` or `end_date` is `400`. | It was ignored (the 30-day default answered). | run |
| `limit` over the maximum is `400`; `limit=0` is `400`. | | run |
| Funnels: default mode `strict_once`; `allow_reentry` without `strict_order` is `400`. | | run |
| Login and setup responses carry `{"status":"ok"}` and the cookie; there is no token in the body. | Read the cookie. | run |
| Sessions, funnel, sequences, flow and entry/exit return `503` while `round_timestamps` is on (`gdpr_mode`). | `/api/stats/main` reports those fields as `null`. | run |

## Breaking: operations

- **Shutdown order.** The server now drains connections first (bounded by
  `shutdown_timeout_secs`), then closes the buffer and flushes (bounded again).
  Older notes said the reverse. The worst case is twice the timeout.
- **Backups.** Stop using a cron job that copies a live `mallard.duckdb`. Use
  [`deploy/backup.sh`](backup-restore.md), which stops the container, copies,
  verifies and always restarts it.
- **Retention** now refreshes the query view and clears the cache, and compaction
  no longer holds the tier lock for the whole merge. No action needed.
- **Tracker.** New attribute `data-link-query="true"` (outbound and download
  events keep the query string and fragment; the default sends origin plus path).
  With `data-hash="true"`, `#/pricing` is reported as the path `/pricing`; a
  fragment that does not start with `/` is not a route. The referrer is sent with
  the first pageview only (later routes send none). The script is about 4 KB
  gzipped (4,008 bytes as served, run). Mallard serves it itself, but browsers and
  CDNs may cache `/mallard.js` for an hour (`Cache-Control: public, max-age=3600`),
  so sites pick up the new script up to an hour after the upgrade.

## Definitions changed: compare with care

These metrics are computed differently from before. Do not draw a trend line across
the upgrade for them.

| Metric | Now |
|---|---|
| Sessions | All events, custom ones included, split by the session window; counted only if there is at least one pageview. |
| Bounce rate | A counted session that is exactly one event and that event is a pageview. A custom event ends the bounce. |
| Visit duration | Last minus first event of a counted session. |
| Views per visit | Mean pageviews per counted session. |
| `views_per_visitor` | `total_pageviews / unique_visitors` (was called `pages_per_visit`). |
| Filters | Select sessions, which are then measured whole (they used to cut sessions into the matching rows). |
| Entry and exit pages | First and last pageview of a counted session. |
| Flow | Pageviews only, consecutive repeats collapsed. |
| `7d` | Seven daily buckets. |
| Retention | First-seen looked up over `visitor_salt_rotation_days` before the range. |

Full definitions with worked examples: [Metric Definitions](metric-definitions.md).

**Unchanged:** unique visitors, pageviews, total events, time series values per
bucket, breakdowns other than entry/exit pages, goals, properties and revenue (for
the same explicit dates).

### A safe way to compare

Compare on **explicit dates for a complete past period**, never on `7d`/`30d`
(their windows moved and "today" keeps changing). To see the effect of the
definition changes on your own data:

1. Stop the instance and copy the data directory (a backup is exactly that).
2. Start the **old** binary on the copy on port 18001 and the **new** one on a second
   copy on port 18002, each with a throwaway `MALLARD_DATA_DIR`.
3. Query both with the same `start_date`/`end_date`:

   ```bash
   for p in 18001 18002; do
     curl -s -H "X-API-Key: $KEY" \
       "http://127.0.0.1:$p/api/stats/main?site_id=example.com&start_date=2026-10-01&end_date=2026-10-07"
   done
   ```

4. `unique_visitors`, `total_pageviews` and `total_events` must be identical.
   Differences in `total_sessions`, `bounce_rate`, `avg_visit_duration_secs` and
   `views_per_visit` are the definition changes above, not data loss.

Keep a note of the upgrade date next to any long-running chart of these four
figures.

## After the upgrade

- `curl /health/detailed`: `status` ok, `cold_tier` `healthy` (or `empty` on a
  fresh install), `behavioral_version` 0.10.0 or newer.
- `curl -H "Authorization: Bearer $TOKEN" /metrics | grep build_info` shows the
  binary and extension versions.
- Load the dashboard: no panel says "Not available".
- If a behavioral panel does, go to
  [Troubleshooting](troubleshooting.md#behavioral-endpoints-answer-503).
