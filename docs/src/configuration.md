# Configuration

Mallard Metrics reads an optional TOML file and `MALLARD_*` environment
variables. Everything has a default, so it starts with no configuration.
This page lists every setting; it was checked against `src/config.rs`,
`src/main.rs` and `mallard-metrics.toml.example`.

## Loading rules

```bash
mallard-metrics                      # defaults + environment
mallard-metrics /etc/mallard/mallard-metrics.toml
```

- **Precedence:** environment variable > TOML file > default.
- **A named file that cannot be read or parsed is a startup error**, never
  silently replaced by defaults.
- **Unknown TOML keys are a startup error** (`unknown field ... expected one of
  ...`), so `retention_dayz = 30` cannot be ignored.
- **A malformed environment value is a warning, not an error:** the log says
  `MALLARD_PORT="abc" is not a valid u16; keeping 8000` and the previous value
  stays. Check the first lines of the log after changing a variable.
  Booleans accept `1/true/yes/on` and `0/false/no/off`, case-insensitive.
- Validation runs before the server binds. A failure prints
  `Configuration error: ...` and exits with status 1.
- `--healthcheck` and `--install-extension` read **only the environment**; a
  TOML file path is not honoured by them (see
  [Troubleshooting](troubleshooting.md#running-the-extension-install-step-offline)).

## Secrets (environment only)

These are never read from the TOML file, so the file can be committed or baked
into an image.

| Variable | Default | Validation and effect |
|---|---|---|
| `MALLARD_SECRET` | auto-generated | Key material for visitor IDs. **At least 16 characters** (counted as characters, after trimming whitespace); a shorter value stops startup with `MALLARD_SECRET is shorter than 16 characters. Use a random value, e.g. openssl rand -hex 32.` Empty or unset: a random UUID is generated once and stored in `data_dir/.secret` (mode 0600); an unreadable existing file also stops startup rather than silently replacing the secret. Changing the secret re-identifies every visitor. |
| `MALLARD_ADMIN_PASSWORD` | unset | At least 12 characters and not one of a short list of obvious values; otherwise startup stops with `MALLARD_ADMIN_PASSWORD rejected: ...`. Takes precedence over a password stored by `/api/auth/setup`; never written to disk. Unset with no stored hash means **reads are open** until someone completes setup. |
| `MALLARD_METRICS_TOKEN` | unset | Bearer token required by `GET /metrics`. Empty counts as unset. |
| `MALLARD_ALERTS_WEBHOOK_SECRET` | unset | Key for the HMAC-SHA256 signature on webhook notifications. At least 16 characters, otherwise startup stops. Unset sends unsigned notifications (advisory). A `webhook_secret` key in the TOML file is rejected as an unknown field. See [Webhook Alerts and Reports](alerts-and-reports.md#request-signing). |
| `RUST_LOG` | `mallard_metrics=info,tower_http=info` | `tracing` filter. |

## Server and storage

| Variable / TOML key | Type | Default | Validation | Effect |
|---|---|---|---|---|
| `MALLARD_HOST` / `host` | string | `0.0.0.0` | bind must succeed | Listen address. Use `127.0.0.1` behind a proxy on the same host. |
| `MALLARD_PORT` / `port` | u16 | `8000` | | Listen port. |
| `MALLARD_DATA_DIR` / `data_dir` | path | `data` | must not contain `* ? [ ]` | Holds `mallard.duckdb`, `events/`, `extensions/`, `.secret`, `admin.json`, `api_keys.json`. The characters are refused because DuckDB would treat them as a glob. |
| `MALLARD_EXTENSION_DIR` / `extension_directory` | path | `data_dir/extensions` | empty = unset | Where `INSTALL behavioral` writes and loads from. The image sets it to `/data/extensions`. DuckDB's `home_directory` is set to `data_dir` too. |
| `MALLARD_RETENTION_DAYS` / `retention_days` | u32 | `0` | | Delete `date=` partitions older than N days; `0` keeps everything. Runs at startup and every 24 hours, clears the query cache and refreshes the view. |
| `MALLARD_COMPACT_AFTER_FILES` / `compact_after_files` | usize | `24` | `0` disables; values below 2 act as 2 | Merge a partition's files of one level once it holds at least this many. |
| `MALLARD_READ_CONNECTIONS` / `read_connections` | usize | `4` | `0` or `1` shares the writer connection | Read-only DuckDB connections serving queries. With a pool a slow dashboard query cannot block ingestion. |
| `MALLARD_DUCKDB_MEMORY_LIMIT` / `duckdb_memory_limit` | string | unset (about 80% of RAM) | number plus `B/KB/KiB/MB/MiB/GB/GiB/TB/TiB` | Cap on query memory, e.g. `1GB`. Set it below the container limit. |
| `MALLARD_DUCKDB_THREADS` / `duckdb_threads` | u32 | unset (one per core) | `0` is an error in TOML; a non-positive env value is a warning | DuckDB worker threads. |
| `MALLARD_GEOIP_DB` / `geoip_db_path` | path | unset | empty = unset; a missing file is not an error | MaxMind GeoLite2-City database; geography is `NULL` without it. |

## Ingestion

| Variable / TOML key | Type | Default | Validation | Effect |
|---|---|---|---|---|
| `MALLARD_FLUSH_COUNT` / `flush_event_count` | usize | `1000` | `> 0` | Flush when the buffer holds this many events. |
| `MALLARD_FLUSH_INTERVAL` / `flush_interval_secs` | u64 | `60` | `> 0` | Also flush at least this often. This is the **delay before an event becomes visible in reports**. |
| `MALLARD_MAX_BUFFERED_EVENTS` / `max_buffered_events` | usize | `100000` | `0` = unbounded; otherwise `>= flush_event_count` | Cap on memory held while flushes fail. Beyond it new events get `503` and `mallard_events_dropped_total` counts them. |
| `MALLARD_FILTER_BOTS` / `filter_bots` | bool | `true` | | Drop known-bot User-Agents (answered `202`). |
| `MALLARD_SITE_IDS` / `site_ids` | comma list / array | empty (any site) | each entry: letters, digits, `. - _ :`, at most 256 characters | Ingest allowlist, enforced against the `Origin` header **and** the payload's `d`. |
| `MALLARD_RATE_LIMIT` / `rate_limit_per_site` | u32 | `0` (off) | | Events per second per site ID; token bucket whose burst equals the rate. |
| `MALLARD_RATE_LIMIT_PER_IP` / `rate_limit_per_ip` | u32 | `0` (off) | | Events per second per client (IPv4 address or IPv6 /64). |
| `MALLARD_MAX_TRACKED_KEYS` / `max_tracked_keys` | usize | `10000` | | Cap on rate-limit buckets and login-attempt records. At the cap, idle entries are dropped first, then the least recently used, in batches (counted in `mallard_rate_limiter_evictions_total`); new keys are never refused. |

## Reverse proxy and cookies

| Variable / TOML key | Type | Default | Validation | Effect |
|---|---|---|---|---|
| `MALLARD_TRUST_PROXY_HEADERS` / `trust_proxy_headers` | bool | `false` | | Take the client address from `X-Forwarded-For` (or `X-Real-IP` when no chain exists) instead of the socket. |
| `MALLARD_TRUSTED_PROXY_HOPS` / `trusted_proxy_hops` | usize | `1` | `1..=8` when `trust_proxy_headers` is on; otherwise unused | The client is entry N **from the right** of `X-Forwarded-For`. |
| `MALLARD_DASHBOARD_ORIGIN` / `dashboard_origin` | string | unset | `http(s)://host[:port]`, no path, no credentials | Restricts dashboard CORS to that origin (with credentials) and enables the CSRF check on login, setup, logout and admin routes. |
| `MALLARD_SECURE_COOKIES` / `secure_cookies` | bool | `false` | | Add `Secure` to the session cookie. Also implied by an `https://` `dashboard_origin`. |

## Queries and sessions

| Variable / TOML key | Type | Default | Validation | Effect |
|---|---|---|---|---|
| `MALLARD_CACHE_TTL` / `cache_ttl_secs` | u64 | `60` | `0` disables | Lifetime of cached report results (all read endpoints except realtime and export). |
| `MALLARD_CACHE_MAX_ENTRIES` / `cache_max_entries` | usize | `10000` | `0` = unbounded | LRU cap on the cache. |
| `MALLARD_MAX_CONCURRENT_QUERIES` / `max_concurrent_queries` | usize | `10` | `0` = unlimited | Query slots. Waiters queue for up to `query_queue_wait_secs`, then get `429`. |
| `MALLARD_QUERY_QUEUE_WAIT_SECS` / `query_queue_wait_secs` | u64 | `5` | `> 0` | How long a request waits for a query slot before it gets `429` (with `Retry-After: 1`). |
| `MALLARD_QUERY_DEADLINE_SECS` / `query_deadline_secs` | u64 | `20` | `> 0` | How long a statement may run before DuckDB interrupts it and the request gets `504` (`query_timeout`). The two budgets together must not exceed the 30 s request timeout, so the query layer answers, with an explanation, before the timeout does; the defaults leave 5 s of margin. |
| `MALLARD_SESSION_WINDOW_MINUTES` / `session_window_minutes` | u32 | `30` | `> 0` | Inactivity gap that ends a visit. |
| `MALLARD_REALTIME_WINDOW_MINUTES` / `realtime_window_minutes` | u32 | `5` | `> 0` | Window `/api/stats/realtime` treats as "now". |

## Dashboard authentication

| Variable / TOML key | Type | Default | Validation | Effect |
|---|---|---|---|---|
| `MALLARD_SESSION_TTL` / `session_ttl_secs` | u64 | `86400` | `> 0` | Session lifetime (sessions are in memory; a restart logs everyone out). |
| `MALLARD_MAX_SESSIONS` / `max_sessions` | usize | `10000` | | Cap on concurrent sessions. |
| `MALLARD_MAX_LOGIN_ATTEMPTS` / `max_login_attempts` | u32 | `5` | `0` disables lockout (logged as an advisory) | Failures before a client is locked out. |
| `MALLARD_LOGIN_LOCKOUT` / `login_lockout_secs` | u64 | `300` | `> 0` (0 is a startup error) | Lockout length. |

## Operations

| Variable / TOML key | Type | Default | Validation | Effect |
|---|---|---|---|---|
| `MALLARD_SHUTDOWN_TIMEOUT` / `shutdown_timeout_secs` | u64 | `30` | at least 1 is used | Bound for connection draining, **and separately** for the final flush. Worst case a stop takes twice this. |
| `MALLARD_LOG_FORMAT` / `log_format` | string | `text` | `text` or `json` | Log encoding. |

## Visitor identity and privacy

| Variable / TOML key | Type | Default | Validation | Effect |
|---|---|---|---|---|
| `MALLARD_VISITOR_SALT_ROTATION_DAYS` / `visitor_salt_rotation_days` | u32 | `1` | `>= 1` | Days a visitor-ID salt stays in use. Also the retention report's lookback. See below. |
| `MALLARD_GDPR_MODE` / `gdpr_mode` | bool | `false` | | Preset: forces `strip_referrer_query`, `round_timestamps`, `suppress_browser_version`, `suppress_os_version`, `suppress_screen_size` on and lowers `geoip_precision` to at most `country`. Not `suppress_visitor_id`. |
| `MALLARD_STRIP_REFERRER_QUERY` / `strip_referrer_query` | bool | `false` | | Store referrers without `?query` and `#fragment`. |
| `MALLARD_ROUND_TIMESTAMPS` / `round_timestamps` | bool | `false` | | Round event time down to the hour. **Disables** session, funnel, sequence, flow and entry/exit reports (`503`). |
| `MALLARD_SUPPRESS_VISITOR_ID` / `suppress_visitor_id` | bool | `false` | | Replace the ID with a random UUID per request: unique visitors become page loads and every visit is one page. |
| `MALLARD_SUPPRESS_BROWSER_VERSION` / `suppress_browser_version` | bool | `false` | | Keep the browser name only. |
| `MALLARD_SUPPRESS_OS_VERSION` / `suppress_os_version` | bool | `false` | | Keep the OS name only. |
| `MALLARD_SUPPRESS_SCREEN_SIZE` / `suppress_screen_size` | bool | `false` | | Drop the width **and** the device type. |
| `MALLARD_GEOIP_PRECISION` / `geoip_precision` | string | `city` | `city`, `region`, `country`, `none` | How much of a GeoIP result is stored. |

## Webhook alerts and reports

The `[alerts]` TOML table and `MALLARD_ALERTS_*` variables configure outbound
webhook notifications. Nothing is sent, and nothing in the table is validated,
until `webhook_url` (`MALLARD_ALERTS_WEBHOOK_URL`) is set. The URL and the
signing secret can be set only here, never through the API. The table has about
twenty keys, listed with defaults and ranges in
[Webhook Alerts and Reports](alerts-and-reports.md#configuration-reference) and
commented in `mallard-metrics.toml.example`. The ones that decide whether it is
safe:

| Variable / TOML key | Default | Effect |
|---|---|---|
| `MALLARD_ALERTS_WEBHOOK_URL` / `webhook_url` | unset | Destination. `https://` only, no `user:pass@`. Treat it as a secret (Slack and Discord URLs are tokens). |
| `MALLARD_ALERTS_ALLOW_HTTP_WEBHOOK` / `allow_http_webhook` | `false` | Without it an `http://` URL stops startup. |
| `MALLARD_ALERTS_ALLOW_PRIVATE_WEBHOOK_TARGETS` / `allow_private_webhook_targets` | `false` | Without it, loopback, private, link-local, CGNAT, multicast, unspecified and cloud-metadata addresses are refused, and a literal one stops startup. |

## Startup advisories

These are logged as warnings and never block startup:

- `gdpr_mode` with `retention_days = 0`.
- `visitor_salt_rotation_days = 1` (explains the cost for multi-day reports).
- `trust_proxy_headers` on (restates the hop rule).
- `max_login_attempts = 0`.
- `site_ids` empty **and** `rate_limit_per_ip = 0`: anyone who can reach the
  server can create sites and write events without limit.
- `suppress_visitor_id` on.
- Webhook alerts configured without `MALLARD_ALERTS_WEBHOOK_SECRET` (unsigned),
  with `allow_private_webhook_targets` or `allow_http_webhook` on, or with traffic
  alerts or reports enabled but no sites to cover.

## Two settings that need a decision

### `trust_proxy_headers` and `trusted_proxy_hops`

The client address feeds the visitor ID, GeoIP, the rate limiters and the login
lockout. Default `false`: the socket's peer address is used.

Behind a reverse proxy that is wrong in the other direction: every visitor
appears to come from the proxy, so unique visitors collapse to distinct
User-Agents, GeoIP never resolves, and five wrong passwords from anyone lock the
admin out. Turn `trust_proxy_headers` on **only** when every request really
passes through your proxy, and tell Mallard how many proxies that is:

> Every proxy **appends** the address it received the connection from to
> `X-Forwarded-For`. Entries to the left were written by whoever came before, so
> only the entries your own infrastructure appended can be believed. Mallard
> takes entry `trusted_proxy_hops` **counting from the right**.

| Topology | `trusted_proxy_hops` |
|---|---|
| Client → Caddy → Mallard (the bundled setup) | `1` |
| Client → nginx with `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;` → Mallard | `1` |
| Client → CDN → nginx → Mallard | `2` |

Verified against a running server with `trusted_proxy_hops = 1`:
`X-Forwarded-For: 9.9.9.9, 203.0.113.7` is rate-limited as `203.0.113.7`, and
replacing the spoofable left entry (`1.2.3.4, 203.0.113.7`) does not change the
client. Too small a value attributes traffic to a proxy; too large a value lets
a client choose its own address. A chain shorter than the hop count is not
believed (the peer address is used), and the value must parse as an IP address
(optionally with a port, bracketed IPv6 allowed); anything else falls back to the
peer. `X-Real-IP` is read only when there is no `X-Forwarded-For` at all.

### `visitor_salt_rotation_days`

Default `1`: the most private setting, and the one that limits what can be
measured. A visitor seen on two UTC days has two unrelated IDs, so

- "Unique visitors" over several days counts visitor-days, not people.
- Weekly retention cohorts read zero past week 0;
  `/api/stats/retention` says so in its `caveat` field.
- A visit spanning UTC midnight is two visits.

Raising it (7, 30) lengthens the life of a pseudonymous identifier, which is a
privacy decision to record. Retention over *N* weeks needs at least `(N - 1) * 7`.
Periods are aligned to the Unix epoch, so every instance sharing a secret agrees
on the boundaries. See [Privacy and GDPR](privacy-gdpr.md#salt-rotation).

## The extension directory

DuckDB installs community extensions under `$HOME/.duckdb/extensions`. The
shipped image has a read-only root filesystem and no writable home, so the image
sets `HOME=/data` and `MALLARD_EXTENSION_DIR=/data/extensions`, and the server
also points DuckDB's `home_directory` at `data_dir`. Without that,
`INSTALL behavioral` fails and funnels, retention, sessions, sequences and flow
answer `503` on an otherwise healthy deployment. Keeping the extension on the data
volume also means it survives container replacement and can be seeded once for a
network that has no outbound route.
