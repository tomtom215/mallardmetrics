# Troubleshooting

Symptom-first. Where a message is quoted it was observed or read from the source
of the 0.2.0-line binary. Start with these two commands:

```bash
curl -s http://localhost:8000/health/detailed
curl -s -H "Authorization: Bearer $MALLARD_METRICS_TOKEN" http://localhost:8000/metrics | grep -v '^#'
```

## The server does not start

The first line says why. All of these exit with status 1.

| Message | Cause and fix |
|---|---|
| `Configuration error: ...` | A rejected setting; the text names it (see [Configuration](configuration.md)). Includes unknown TOML keys, `login_lockout_secs = 0`, a `trusted_proxy_hops` outside 1 to 8, `max_buffered_events` below `flush_event_count`, a bad `geoip_precision`, `log_format`, `dashboard_origin` or `duckdb_memory_limit`, and a data directory containing `* ? [ ]`. |
| `failed to read config file ...` / `failed to parse config file ...` | The file named on the command line. It is never silently replaced by defaults. |
| `MALLARD_SECRET is shorter than 16 characters ...` | Use a longer secret, e.g. `openssl rand -hex 32`. |
| `MALLARD_ADMIN_PASSWORD rejected: ...` | Under 12 characters or too common. |
| `Cannot read .../.secret: ...; refusing to replace the visitor-ID secret` | Permissions on the data directory. The container runs as UID 65532: `chown -R 65532:65532 <data dir>`. |
| `Startup failed: could not open ...mallard.duckdb` / `migrations failed` / the database is at a newer schema | Wrong directory or permissions; a database written by a newer build cannot be opened by an older one. |
| `Failed to create the data directory ...` | The path is not writable by the process user. |
| `Failed to bind to 0.0.0.0:8000: ...` | The port is in use, or `MALLARD_HOST` is not a local address. |
| `could not create the extension directory ...` | `MALLARD_EXTENSION_DIR` is not writable. |

A *malformed* environment value such as `MALLARD_PORT=abc` does **not** stop the
server: the log says `MALLARD_PORT="abc" is not a valid u16; keeping 8000` and
the default is used. If a setting seems ignored, read the first lines of the log.

## Behavioral endpoints answer 503

Funnels, retention (and the dashboard's Funnel, Retention, Sequence and Flow
panels), sessions, sequences, flow and entry/exit pages answer `503`
(`code: behavioral_unavailable`) and `/api/stats/main` returns `null` for
`total_sessions`, `bounce_rate`, `avg_visit_duration_secs` and `views_per_visit`.
The extension is also retried in the background every 60 s, so
`behavioral_extension_loaded` in `/health/detailed` can turn `true` after boot
when the first download failed (for example the network came up late). Check:

```bash
curl -s http://localhost:8000/health/detailed | grep behavioral
# "behavioral_extension_loaded": false, "behavioral_version": null
```

and the log for `Behavioral extension unavailable; funnels, retention, sessions,
sequences and flow analysis will report 503 until it loads error=...`.

| Cause | The `error=` text | Fix |
|---|---|---|
| **`gdpr_mode` or `round_timestamps` is on** | the endpoint's own message: `... is not available while timestamps are rounded to the hour` (the extension *is* loaded) | Use the individual privacy flags instead of `gdpr_mode`, or accept the loss. |
| **No outbound network on first run** | `IO Error: Failed to download extension "behavioral" at URL "http://community-extensions.duckdb.org/v1.5.6/linux_amd64/behavioral.duckdb_extension.gz" ... (ERROR Could not establish connection)` | The server still starts and serves everything else. Seed the extension once: [below](#running-the-extension-install-step-offline). The production compose file puts Mallard on an internal network on purpose, so this is the expected state there until you seed. |
| **Unsupported platform: musl / Alpine** | the same download error, with a `..._musl` URL | There is no fix: the extension is not published for musl. Use the glibc image or a `*-unknown-linux-gnu` binary. |
| **Cached extension older than 0.10.0** | `behavioral extension 0.9.1 is older than the required 0.10.0; delete the cached behavioral.duckdb_extension ...` | [Clean the cache](upgrading.md#cleaning-the-extension-cache). |
| **Extension directory not writable** | `could not create the extension directory`, or `IO Error: Can't find the home directory` | Set `MALLARD_EXTENSION_DIR` to a writable path on the data volume; in a container keep `HOME=/data`. |

**Platform matrix (checked 2026-10-21 with `HEAD` requests for DuckDB v1.5.6):**
`linux_amd64`, `linux_arm64`, `osx_amd64`, `osx_arm64` answer `200`;
`linux_amd64_musl` and `linux_arm64_musl` answer `404`. The repository
publishes per DuckDB version, so a DuckDB upgrade needs a fresh download.

Egress firewalls: the URL DuckDB requests is `http://community-extensions.duckdb.org/...`
(plain HTTP, port 80, as shown in the error above; the server's log message that
says "HTTPS" is imprecise). Allow that host, or seed offline.

## Running the extension install step offline

`mallard-metrics --install-extension` downloads the extension into the extension
directory and exits. It creates no database, so it is safe against a live data
directory. Verified:

```
$ MALLARD_DATA_DIR=/data mallard-metrics --install-extension
Installed the behavioral extension (version 0.10.0) into /data/extensions
Start the server normally; it will load the extension from disk.
```

and, with no route to the repository (run in an empty network namespace):

```
Could not install the behavioral extension: IO Error: Failed to download extension "behavioral" at URL "http://community-extensions.duckdb.org/v1.5.6/linux_amd64/behavioral.duckdb_extension.gz"
...
This step needs outbound HTTP (plain port 80, not HTTPS) to community-extensions.duckdb.org. A firewall that only allows egress on 443 blocks it. Run it from a host or network that has one, against the same data directory.
```

(exit status 0 and 1.) Things to know:

- It reads **only environment variables**, never a TOML file: put the data
  directory in `MALLARD_DATA_DIR` (and `MALLARD_EXTENSION_DIR` if you changed it)
  exactly as the service uses it. `--healthcheck` behaves the same way.
- Run it on a machine **of the same DuckDB version and platform** as the server.
  The files are `extensions/v<duckdb>/<platform>/behavioral.duckdb_extension` and
  its `.info`; neither is host-specific.
- Docker, isolated network (the production compose layout):

  ```bash
  sudo docker run --rm --user 65532:65532 -v /srv/mallard/data:/data \
    mallard-metrics:latest --install-extension
  docker compose -f deploy/docker-compose.production.yml restart mallard
  ```

  The image already sets `MALLARD_DATA_DIR=/data` and
  `MALLARD_EXTENSION_DIR=/data/extensions`, and docker's default bridge has a route
  (the compose `proxy` network does not). `setup.sh` does this step for you.
- Bare metal or systemd: run it with the unit's environment
  (`sudo -u <user> env MALLARD_DATA_DIR=/var/lib/mallard-metrics mallard-metrics --install-extension`),
  then restart the service.
- Fully air-gapped: run it anywhere with access and copy the whole
  `<data dir>/extensions/` directory (the extension and its `.info` file) to the
  server, then restart.
- Re-run after any upgrade that changes the bundled DuckDB version.

## Dashboard or reports show no data

1. **The flush delay.** Events are invisible until flushed, up to
   `flush_interval_secs` (default 60) after they arrive. `buffered_events` on
   `/health/detailed` counts them. Wait, or lower the interval.
2. **The tracker never fired.** It skips `localhost`, `file:` and private
   addresses unless `data-include-local="true"`, and respects `data-exclude`,
   `data-honor-dnt` and the `mallard_ignore` opt-out key. Check the browser's
   network tab for `POST /api/event`.
3. **Ingest answered something other than 202.** `403` = `site_ids` mismatch
   (the `data-domain` value, and the Origin host); `400` = invalid site ID or an
   empty/over-long field; `429` = rate limit; `413`/`415`/`422` = a malformed
   direct call. See [Ingestion](api-reference/ingestion.md#errors).
4. **A `202` that recorded nothing** means the event was classed as a bot
   (`filter_bots`, including curl's default User-Agent) or came through the pixel
   route with a missing parameter. `mallard_events_ingested_total` shows what was
   really buffered.
5. **Wrong site selected or period.** `GET /api/sites` lists what the server
   knows; `period=7d` ends today in UTC.
6. **`cold_tier` is `degraded`**: see below.

## Degraded cold tier

`/health/detailed` returns `503` with `"status": "degraded"` and
`"cold_tier": "degraded"`: Parquet files exist but `events_all` does not read
them, or reads a pattern that matches nothing. Reports then omit everything
that was flushed, while ingestion and `/health` keep succeeding; `/health/ready`
may still say `ready`. Typical causes: the view could not be built at startup
(look for `Could not create the events_all view; queries will be limited`),
the data directory was swapped or restored underneath a running process, or a
directory was deleted by hand. First fix the cause (permissions, the mount),
then **restart**: the view is rebuilt at startup and again after each flush that
writes a file. Also check `mallard_last_flush_success_timestamp_seconds`.
`cold_tier: "empty"` is normal on a fresh install.

## 429 and 504 mean different things

| Response | Where | Meaning | Action |
|---|---|---|---|
| `429` with `Retry-After: 1`, empty body | `POST`/`GET /api/event` | The per-site or per-IP rate limit. | Raise `rate_limit_per_site`/`per_ip` if real traffic is being cut (it counts in `mallard_rate_limit_rejections_total`); behind a proxy make sure `trust_proxy_headers` is on, or all visitors share one bucket. |
| `429` `code: too_many_requests` | `/api/stats/*` | All `max_concurrent_queries` slots were busy for `query_queue_wait_secs` (default 5 s). | Retry; reduce panel fan-out; raise the limit if CPU allows. |
| `429` with `Retry-After: <seconds>`, `{"error":"Too many failed attempts..."}` | login / setup | Lockout of that address. Behind a proxy without `trust_proxy_headers` it hits **everyone**. | Wait, or fix the proxy settings. |
| `504` `code: query_timeout` | `/api/stats/*` | A statement ran `query_deadline_secs` (default 20 s) and was interrupted. | Shorter date range, fewer filters; more `duckdb_threads`/memory; run `compact_after_files` (many small files slow every query). |
| `408` | any | The 30 s request timeout. | Same as above, or a slow client. |
| Connection closed with no response | any | The client sent nothing, or only part of a request head, for 30 s; idle keep-alive connections close at the same limit. HTTP/1 only. | A proxy must not hold idle upstream connections longer than 30 s without reuse; set its upstream keep-alive timeout below that. |
| `503` ingest | `POST /api/event` | The buffer is at `max_buffered_events` (flushes failing: see disk full) or the server is shutting down. | Fix the flush; the tracker does not retry. |
| `503` login `Server is busy` | `/api/auth/login` | Four password checks already running. | Retry; a flood of login attempts. |

## Disk full

Symptoms: `Periodic flush failed` errors in the log;
`mallard_flush_failures_total` or `mallard_buffer_flush_failures_total` rising;
`mallard_last_flush_success_timestamp_seconds` stuck; `buffered_events` growing
toward `max_buffered_events`; then `503` from ingest and
`mallard_events_dropped_total` rising (**data loss**).

What is safe: events that were already in the buffer are put back and retried
(nothing is lost until the cap), a half-written Parquet file is only ever a
`.pending` or `.tmp` file that queries cannot see, and the rows are kept in the hot
table. To recover: free space (lower `retention_days`, delete old partitions you
no longer need, grow the volume, `docker system prune` for image layers), then
watch the next flush succeed. No restart is needed; the next flush moves the stranded
rows to Parquet. If the process was killed meanwhile, events still in the memory
buffer are gone.

## Restart and recovery

A restart after a crash is automatic and needs no flags. At startup, in order, the
log shows:

1. `Cleaned up interrupted Parquet writes removed=N` (stale `.tmp` files, and
   compaction leftovers resolved).
2. `Checked for interrupted flushes report=RecoveryReport { published, discarded, quarantined }`
   and, when anything was non-zero, a warning `Recovered Parquet flushes
   interrupted by a crash`. `published` staged files were the only copy and were
   published; `discarded` were duplicates of rows still in the hot table;
   `quarantined` are unreadable files set aside as `*.pending.corrupt`, which need
   a human look and are never deleted automatically.
3. The events_all view is built, the read pool opens (`Read connection pool
   ready`), and the server listens.

What a crash can lose: the in-memory buffer (up to `flush_interval_secs` of
events). Flushed data is crash-safe. A graceful `SIGTERM` loses nothing (`Flushed
remaining events during shutdown count=N`). In containers give it time:
`stop_grace_period: 45s`; the log line `Connections did not drain in time;
shutting down with some still open` or `Graceful shutdown flush timed out; some
buffered events may be lost` means the timeout was too short.

## Reading the logs

Logs go to stderr; set `MALLARD_LOG_FORMAT=json` for one object per line and
`NO_COLOR=1` for text without escape codes. Messages worth knowing:

| Level | Message | Meaning |
|---|---|---|
| WARN | `visitor_salt_rotation_days=1 ...`, `site_ids is empty and rate_limit_per_ip is 0 ...`, `trust_proxy_headers is enabled ...` | Startup advisories, not faults. |
| WARN | `Behavioral extension unavailable ...` | See above. |
| WARN | `Analytics query exceeded its deadline; interrupting` | A `504` was produced. |
| WARN | `Request from a locked-out IP` / `Admin login failed: invalid password` | Lockout and failed logins (addresses are shown only as an anonymised prefix). |
| WARN | `Admin endpoint refused: no admin password is configured yet` | Someone called an admin route before setup. |
| WARN | `CSRF check failed on admin endpoint` | `dashboard_origin` mismatch. |
| ERROR | `Periodic flush failed` / `Flush failed; the event stays buffered or in the hot table` | A flush error; see Disk full. |
| ERROR | `GDPR erasure could not remove a partition` | An erase answered `partial`; repeat it. |
| ERROR | `A staged Parquet file holds the only copy of flushed events and could not be published` | Recovery could not rename a file; fix permissions and restart. |
| WARN | `Parquet compaction failed` | An optimisation failed; the flush itself succeeded. |
| INFO | `Periodic flush completed count=N`, `Data retention cleanup completed removed=N` | Normal. |

## Everyone is locked out of the dashboard, or all visitors look like one

Mallard sees only the proxy's address. Set `MALLARD_TRUST_PROXY_HEADERS=true`
and `MALLARD_TRUSTED_PROXY_HOPS` to the number of proxies you own (1 for Caddy or
nginx, 2 behind a CDN). Wait for the lockout (`login_lockout_secs`) or restart,
which clears it. See [Configuration](configuration.md#trust_proxy_headers-and-trusted_proxy_hops).

## Cannot log in after setting a password

`MALLARD_ADMIN_PASSWORD`, when set, **overrides** a password stored by the setup
endpoint (`admin.json`), so an old environment value wins. To change the
password, change the environment variable and restart. Removing both the variable
and `admin.json` puts the instance back in first-run mode, where **the first
visitor to reach `/api/auth/setup` can claim it**: do that only while it is not
reachable.

## A share link says "This link no longer works"

The server answers `401 share_link_invalid` for every kind of failure, on
purpose: the link was revoked, it expired, it was mistyped or cut off (a token is
`ms_` and 64 hex digits, and the part after `#share=` is the whole secret), or the
instance lost it. Check **Share links** in the admin dashboard for its status. A
link also stops working when `share_links.json` could not be read at start: the
file is moved to `share_links.json.corrupt-<time>` and the store starts empty (see
[Share Links](share-links.md#lifetime-and-storage)); create the links again, or
inspect the moved file. A link restored from an old backup is valid again even if
you revoked it afterwards.

`429` on a share link means its own rate limit (30 requests per second) or the
instance-wide one (120 per second) was reached; the dashboard retries on its own.
