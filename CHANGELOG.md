# Changelog

All notable changes to Mallard Metrics will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## Table of Contents

- [Unreleased](#unreleased)
- [0.2.0](#020---2026-08-31)
- [0.1.0](#010---2026-04-11)

---

## [Unreleased]

## [0.2.0] - 2026-08-31

A broad modernisation pass. The toolchain, edition and every dependency are
current. Several analytics results that were quietly wrong are corrected —
funnels were inverted, retention measured "did anybody come back" rather than
how many, and the realtime endpoint could not run at all. The data the ingest
path has always collected is now queryable, and every report can be narrowed to
a segment. Two of the bugs fixed here were invisible to a fully green test
suite and were found by a new end-to-end check that runs the actual binary.

A second review pass then found four defects that a deployment would have hit
rather than a test: an instance could be claimed by a stranger after a restart,
five bad logins locked out everybody, reports occasionally double-counted, and
the behavioral extension could not install in the container the README
recommends.

### Fixed — takeover, lockout and consistency

- **An admin password set through `/api/auth/setup` was never persisted.** It
  lived in a `Mutex<Option<String>>` and nothing wrote it anywhere, so a
  restart put the instance back into first-run mode: `setup_required` was true
  again and whoever reached it first — not necessarily the operator — could
  claim it and mint an admin API key. The Argon2id hash is now written to
  `data_dir/admin.json` at mode 0600 and loaded at startup.
  `MALLARD_ADMIN_PASSWORD` still takes precedence and is deliberately not
  written back, so removing the variable does not leave a stale credential.
  A setup request that cannot persist the hash now fails rather than accepting
  a password it would silently lose.
- **Brute-force protection was global, not per-IP.** The login and setup
  handlers resolved the client address with the peer socket omitted, so on any
  deployment not behind a trusted proxy — the default — every attempt was keyed
  by the literal string `"unknown"`. Five failures from anyone locked *everyone*
  out for the lockout period, and an attacker rotating source addresses gained
  nothing because the addresses were never distinguished. The handlers now take
  the peer address.
- **Reports could double-count events during a flush or a compaction.**
  `events_all` is the hot table `UNION ALL` a live Parquet glob, re-expanded on
  every query. A flush writes the Parquet file and only then deletes the rows it
  copied; compaction renames the merged file into the glob and only then removes
  its sources. A query landing inside either window saw the same rows on both
  sides. A new `TierLock` gives writers exclusive access for the length of that
  window and readers shared access, so no query can observe it. The regression
  test reproduces the old behaviour reliably — 180 rows where 120 were written.
- **The bulk insert was not atomic.** DuckDB's appender commits whenever an
  internal chunk fills, so a failure part-way through a flush left the earlier
  chunks durably inserted while the buffer restored every event for the next
  attempt — duplicating everything up to the failure point. The batch is now one
  explicit transaction, rolled back on failure.
- **A failed hot-table delete left permanent duplicates.** If the `DELETE` after
  a Parquet write failed, the rows existed in both tiers and nothing ever
  retried, so every subsequent query counted them twice, forever. The Parquet
  file is now removed, restoring the partition so the next flush retries it.
- **One oversized revenue value could stop ingestion entirely.**
  `revenue_amount` is `DECIMAL(12,2)`; a value past its range is not clipped on
  insert, it fails the whole bulk append. A single event carrying `1e30` would
  therefore fail every flush, fill the buffer and start discarding unrelated
  events. Out-of-range and non-finite amounts are now dropped at ingest, keeping
  the rest of the event.
- **GDPR erasure ignored buffered events.** Events already in the buffer for the
  erased range were written out by the next flush, minutes after the endpoint
  reported success. The buffer is now flushed first.
- **A newer database could be opened by an older build.** Migrations ran happily
  against a schema version they did not understand, which is how a rollback
  becomes data loss. Startup now refuses, naming the cause.

### Fixed — deployment

- **The behavioral extension could not install in the shipped container.**
  DuckDB installs community extensions under `$HOME/.duckdb/extensions`; the
  `FROM scratch` image sets no `HOME`, and the production compose file runs with
  a read-only root filesystem. `INSTALL behavioral FROM community` failed with
  `IO Error: Can't find the home directory at ''`, so funnels, retention,
  sessions, sequences and flow answered 503 on a deployment that looked healthy
  in every other respect. Both `home_directory` and `extension_directory` are
  now set to the writable data volume — DuckDB resolves the home directory
  during an install even when the extension directory points elsewhere, so
  setting only the latter is not enough. The location is configurable through
  `MALLARD_EXTENSION_DIR`.
- **Every documented `docker pull` command named an image that does not exist.**
  The README and docs said `ghcr.io/tomtom215/mallard-metrics`; the release
  workflow publishes `ghcr.io/tomtom215/mallardmetrics`.
- **The released image had no healthcheck and no OCI labels.** The release
  workflow writes its own Dockerfile from the pre-built binaries and had drifted
  from the from-source one, so GHCR could not link the image back to this
  repository and `docker run` had no readiness signal — on an image with no
  shell, where the binary's own `--healthcheck` is the only option.
- **The `scratch` image had no CA certificates.** DuckDB downloads the
  `behavioral` community extension over HTTPS on first run, and an image built
  `FROM scratch` carries no trust store, so that request had nothing to verify
  the server against. Both the from-source and the release image now copy the
  distribution's ~200 KB root bundle.
- **Dashboard assets were cached for a year at unversioned URLs.** `/app.js` and
  `/style.css` were served `immutable, max-age=31536000`, so upgrading the
  binary changed nothing a returning visitor saw. `index.html` being `no-cache`
  did not help — it pointed at the same URLs. They now revalidate, and the
  `If-None-Match` header is finally read, so a revalidation returns a 304 with
  no body instead of the whole file as it did before.

### Fixed — the dashboard

- **The first-run password prompt did not exist.** `GET /api/auth/status`
  reports `authenticated: true` before setup — reads are open then, which is a
  deliberate deployment mode — and the shell showed the login form only when
  `authenticated` was false. So the form was unreachable: the README promised
  "on first visit you will be prompted to set an admin password" and the first
  visit showed an open dashboard with no prompt and no way to find one. An
  operator had to `POST /api/auth/setup` by hand or restart with
  `MALLARD_ADMIN_PASSWORD`. The dashboard now carries a warning banner saying
  the instance is unprotected, with a button that opens the setup form.
- **No favicon.** The catch-all asset route answered `404` for
  `/favicon.ico` on every page load and the browser tab showed a blank icon.
- **A comparison against an empty period looked broken.** The note named the
  dates but no percentages appeared beside them, because a baseline of zero has
  no meaningful percentage. It now says so.
- **The demo seed script's data was invisible.** `scripts/seed-demo-data.py`
  hard-coded its end date, so every run after that date produced seven thousand
  events outside "Last 30 days" — the dashboard came up empty and the script
  looked broken. The window now ends today.

### Added — features

- **Period comparison.** `/api/stats/main` accepts `compare=previous_period` or
  `compare=year_over_year` and returns a `comparison` object with the same
  headline figures for an equally long earlier window, plus the dates it covers.
  The dashboard renders per-metric change, with the arrow as well as the colour
  carrying the direction, and treats a fall in bounce rate as the good news.
- **API key management in the dashboard.** `POST/GET/DELETE /api/keys` shipped
  with no way to reach them, so issuing a key meant hand-writing a curl command
  with a session cookie pasted out of devtools. Keys can now be created, listed
  and revoked from the UI; the plaintext is shown once and stays on screen until
  dismissed.
- **Segment filters on the realtime endpoint.** It accepted `filters` and
  silently ignored them while the documentation promised every stats endpoint
  honoured them, so a dashboard filtered to one country still showed everyone as
  "right now". The per-minute series is filtered too, so it agrees with the
  totals beside it.
- **HTTP metrics.** `/metrics` exported internal counters and nothing about the
  traffic being served, so "is the dashboard slow?" and "are we returning 500s?"
  needed a separate exporter — which the single-binary deployment exists to
  avoid. It now exports `mallard_http_requests_total` by status class and a
  `mallard_http_request_duration_seconds` histogram on the conventional bucket
  spread.
- **DuckDB resource limits.** `duckdb_memory_limit` and `duckdb_threads` are
  configurable. DuckDB otherwise claims roughly 80% of system RAM and every
  core, so one expensive analytics query could get the process OOM-killed,
  taking ingestion down with it.

### Changed — security hardening

- Constant-time secret comparison uses `subtle` rather than a hand-rolled fold.
  The old version was constant-time only as long as LLVM chose not to turn it
  into an early-exit loop, and that guarantee is the entire point of the
  function.

### Fixed — analytics correctness

- **Funnels were inverted.** `window_funnel` returns the furthest step each
  visitor reached, and the report grouped by that value directly — so it showed
  "visitors who stopped at exactly step N" under a heading that promised the
  funnel. A visitor who converted all the way through was not counted at step 1
  at all, and the dashboard then normalised the bars against the largest of
  those counts, so the percentages were wrong too. Funnels are now cumulative
  (visitors reaching *at least* each step) and carry a conversion rate and a
  drop-off count per step.
- **Retention measured the wrong thing.** The query grouped only by cohort week,
  so the extension's `retention()` aggregated every visitor in the cohort into a
  single boolean array: an entry read `true` if *any one* visitor returned. For
  any active site that was `true` almost everywhere. Cohorts now group per
  visitor and report cohort size, retained counts and retention rates.
- **Retention's week range could not work.** The API advertised 1–52 weeks while
  the extension accepts 2–32 conditions, so `weeks=1` and `weeks>32` produced a
  binder error that was swallowed and reported as "no data". The range is now
  enforced and documented.
- **Timestamps were truncated to whole seconds** on the way into DuckDB, because
  they were formatted as `%Y-%m-%d %H:%M:%S` rather than bound as a typed value.
  That destroyed the ordering of events arriving within the same second — which
  is exactly the ordering `sessionize`, `window_funnel` and `sequence_match`
  depend on. Timestamps now keep microsecond precision.
- **Time series had holes.** Buckets with no events were omitted entirely, so a
  chart drawn from the result connected the days either side of an outage and
  implied traffic that never happened. Every bucket in range is now returned.
- **UTM values were stored raw.** `winter%20sale`, `winter+sale` and
  `winter sale` were three different campaigns in the breakdown. Values are now
  percent-decoded, and UTM keys are matched case-insensitively.
- **Client Hints reported Chrome's frozen version.** Chrome sends both
  `"Chromium";v="<frozen>"` and `"Google Chrome";v="<real>"`, and the brand
  chosen was whichever came first — so the hint, whose entire purpose is to
  carry the real major version past the frozen User-Agent, was throwing that
  version away. Brands are now ranked by specificity: a product brand such as
  Edge or Opera, then `Google Chrome`, then the generic `Chromium`.
- **Referrers were matched by substring**, so `notgoogle.com` was attributed to
  Google and `google.com.phishing.example` looked like organic search. Matching
  is now anchored to the registrable domain, and the source list is larger.
- **The retention cohort query scanned all history** for the site on every
  request. It is now bounded by the query window, and only cohorts that formed
  inside the window are reported.
- **`/api/stats/export` repeated one value on every row.** It computed a single
  top page and top source for the whole period and wrote those two strings into
  every daily row, so columns that looked per-day carried nothing. Each row now
  carries that day's own leader, and a raw per-event export was added.
- **Explicit date ranges dropped their last day**: the inclusive `end_date` was
  passed straight through as the exclusive upper bound.
- Trailing slashes are collapsed, so `/about` and `/about/` are one page rather
  than two rows each holding half the traffic.
- Relative URLs kept their first path segment; `sanitize_pathname` previously
  discarded it as if it were an authority.
- Behavioral endpoints returned `200` with an empty body when the extension was
  missing, which is indistinguishable from a site with no data. They now return
  `503` with an explanation, and `/stats/main` reports session-derived figures as
  `null` rather than `0.0`.

### Fixed — identity, configuration and storage

- **`visitor_id` was not scoped per site**, so one person visiting two sites on
  the same instance produced the same identifier on both, allowing cross-site
  correlation by anyone who could read the stored data. The site ID is now part
  of the HMAC input. This does not change any metric — every query already
  filters by site — but it does re-identify existing visitors once.
- **The client IP was never derived from the connection.** Without a reverse
  proxy neither `X-Forwarded-For` nor `X-Real-IP` is present, and the code
  returned the literal string `"unknown"` for every request — so every visitor
  sharing a User-Agent collapsed into a single visitor ID and unique-visitor
  counts on directly-reachable deployments were meaningless. The peer address is
  now used, and proxy headers are trusted only when `trust_proxy_headers` is on.
- **`site_ids` was only checked against the `Origin` header**, which non-browser
  clients simply omit — so anyone could post events for any site ID and create
  partitions on disk for domains the operator never configured. The allowlist is
  now enforced against the payload too.
- **`log_format` had no effect.** The field existed, was documented and was
  parsed, but the subscriber was built from the environment variable alone, so
  `log_format = "json"` in a TOML file did nothing.
- **`gdpr_mode` did not reduce `geoip_precision = "region"`.** Region is country
  *plus* subdivision — more granular than country, not less — but it was left
  untouched on the mistaken belief that it was already stricter.
- **Parquet writes were not atomic.** `COPY TO` streams into the destination, so
  a crash or a full disk mid-write left a truncated file that the read glob
  picked up, breaking *every* subsequent query until an operator found and
  deleted it. Writes now go to a temporary name and are renamed into place, and
  leftovers from an interrupted write are cleaned up at startup.
- **`migrate_v1` recorded `CURRENT_VERSION` instead of `1`**, so adding a v2
  migration would have made every v1 database claim to be at v2 and skip it.
- **The realtime endpoint could not run at all.** Its window was expressed as
  `NOW() - INTERVAL '<n> minutes'`, but `NOW()` returns `TIMESTAMP WITH TIME
  ZONE`, and interval arithmetic on that type is implemented by DuckDB's ICU
  extension, which this build does not load — so every request failed at bind
  time and returned `500`. Casting back to a naive timestamp would not have been
  a fix either: that cast follows the session time zone, which follows the
  host's locale, while every stored timestamp is naive UTC — so on a server not
  set to UTC the window would have silently slid by the UTC offset. The window
  is now computed once in Rust and bound as a parameter, which also keeps the
  four sub-queries consistent with each other, and the window is bounded above
  as well as below so a client with a skewed clock cannot inflate "right now".
- **`schema_version.applied_at` recorded host-local time** through a
  `DEFAULT CURRENT_TIMESTAMP`, disagreeing with every other timestamp in the
  database. It is now bound as UTC.
- **`dashboard_origin` validation rejected every valid origin.** The check for a
  path counted `/` across the whole string, and `https://host` carries two of
  its own — so a correctly configured dashboard failed to start. The authority
  is now validated as a real `host[:port]`, which also catches values a
  header-value check accepts but no browser ever sends, such as
  `https://exa mple.com`.
- Boolean environment variables were parsed as "anything that is not `0` or
  `false`", so `MALLARD_FILTER_BOTS=no` enabled bot filtering. Parsing is now
  strict, and an unrecognised value warns instead of guessing.
- An unparsable `dashboard_origin` fell back to `*`, which tower-http rejects
  alongside `Allow-Credentials: true` — turning a configuration typo into a
  panic on the first cross-origin request. It is now validated at startup.
- A malformed or missing config file passed on the command line fell back to
  defaults with only a log line, silently disabling retention or the site
  allowlist. It is now a startup error, and unknown keys are rejected so a typo
  such as `retention_dayz` cannot pass unnoticed.
- **`anonymize_ip` redacted nothing for most IPv6 addresses.** It split the
  string on `:` and kept the first four fields, but `::` compression means a
  whole address such as `2001:db8::1` *has* only four fields — so the log line
  reproduced it in full. Addresses are now parsed and masked to the /24 (IPv4)
  or /48 (IPv6) prefix, and a value that does not parse logs a fixed placeholder
  instead of being echoed into the log.

### Fixed — security and resource limits

- **`LoginAttemptTracker::cleanup` freed nothing.** It retained every entry with
  `fail_count > 0`, which is every entry it ever created, so failed logins from
  rotating source addresses grew the map without bound.
- **Argon2 verification ran on the async runtime while holding a mutex**, so
  every login serialised behind the last one and a handful of concurrent
  attempts could stall the whole server. Hashing and verification now run on the
  blocking pool, and the stored hash is cloned rather than held.
- **`api_keys.json` and the visitor-ID secret were written non-atomically and
  world-readable.** Anyone able to read the secret can re-derive every visitor
  ID from an IP and User-Agent. Both are now written to a temporary file with
  mode 0600 and renamed into place.
- The event buffer, the rate-limiter bucket map, the login-attempt map and the
  session store are now bounded. A persistently failing flush previously grew
  the retry buffer until the process was OOM-killed.
- `/api/auth/setup` is unauthenticated by design but had no rate limit, so the
  window between first boot and first configuration could be probed freely. It
  now shares the login endpoint's per-IP protection.
- **Admin endpoints are no longer open before setup.** Open-access mode — no
  admin password configured yet — bypassed the admin middleware as well as the
  read middleware, so anyone who could reach a freshly started instance could
  call `POST /api/keys` or `DELETE /api/gdpr/erase`. Minting a key was the worse
  half: an admin key issued in that window keeps working after the operator
  finishes setup, turning a few unconfigured minutes into permanent access.
  Those routes now return `401` until an admin exists. Analytics reads still
  follow the open-access rule, which is a deliberate deployment mode.
- The minimum admin password length rose from 8 to 12, and a short list of
  obvious passwords is refused.
- Added a per-IP ingest rate limit. A per-site budget alone lets one abusive
  client consume the whole allowance and deny service to a site's real visitors.
- **Proxy headers must now parse as an IP address.** With `trust_proxy_headers`
  enabled, `X-Forwarded-For` and `X-Real-IP` were passed through verbatim — and
  that value becomes a rate-limiter map key, a GeoIP lookup input and part of
  the visitor ID. A request that reached the server without traversing the
  proxy could therefore put arbitrary text of arbitrary length into all three.
  Anything that does not parse now falls through to the peer address, which
  cannot be forged. Parsing also canonicalises the value, so `::1`,
  `0:0:0:0:0:0:0:1` and `[::1]:443` no longer look like three different
  visitors.
- Custom properties are validated as JSON objects before storage; anything else
  would have broken every JSON query on that site's `props` column.
- `revenue_currency` was truncated to three characters, turning `DOLLARS` into
  `DOL`; it is now validated as an ISO 4217 alphabetic code and uppercased.
- The `X-Request-ID` header is echoed back, so an over-long or non-printable
  upstream value is now replaced rather than reflected.
- The logout cookie now carries the same `Secure` attribute as the cookie it
  clears, which a browser requires for the two to match.
- GDPR erasure now invalidates that site's cached query results; the dashboard
  would otherwise have kept serving erased data for the whole cache TTL.
- **Two sites could be served each other's cached results.** Cache keys joined
  their components with `:`, but a site ID may contain `:` (`example.com:8080`
  is valid) and so may a part — so `("main", "a:b", ["p"])` and
  `("main", "a", ["b", "p"])` produced the same key. Components are now
  length-prefixed, which makes the encoding injective. Erasure invalidation had
  the mirror-image bug: it matched `":{site_id}:"` as a substring of the key,
  so erasing `b.com` also cleared `a.com:b.com` while erasing `a.com` missed
  them. The owning site is now stored on the entry and compared exactly.

### Added

- **`GET /api/sites`** — the site IDs that have data. The dashboard previously
  had no way to discover them, so an operator had to remember and retype each.
- **`GET /api/stats/realtime`** — visitors, pageviews, top pages and sources in
  the last few minutes, with a per-minute series.
- **`GET /api/stats/revenue`** — revenue by currency, event and page. Revenue has
  been accepted at ingest and stored since the first release, and nothing ever
  read it back. Currencies are reported separately and never summed together.
- **`GET /api/stats/goals`**, **`/stats/properties`** and
  **`/stats/property-values`** — conversions for every custom event, and
  drill-down into the custom properties attached to them. Both were ingested
  with no query path.
- **Fourteen new breakdown dimensions**, taking the total to twenty — every
  column the ingest path populates: `referrers`, `regions`, `cities`,
  `browser-versions`, `os-versions`, `screen-sizes`, `utm-sources`,
  `utm-mediums`, `utm-campaigns`, `utm-contents`, `utm-terms`, `events`, plus
  session-derived `entry-pages` and `exit-pages`. One
  `/api/stats/breakdown/{dimension}` route replaces the six near-identical
  handlers.
- **Segment filters.** A `filters` parameter narrows every figure a request
  returns — headline metrics, the time series, breakdowns, goals, revenue,
  exports and the behavioral reports alike. Until now a report could only ever
  describe a whole site, so "how do German visitors move through the funnel"
  had no answer at all.

  Conditions are written `dimension==value` or `dimension!=value` and joined by
  `;` (not `,`, because campaign names contain commas). Dimension names are the
  breakdown slugs, so a row in a breakdown becomes a filter without a second
  vocabulary — and the dashboard makes those rows clickable, with a chip per
  active condition.

  `(unknown)` matches events where the value was not recorded, which is exactly
  what a breakdown displays for `NULL`. Negation is NULL-safe in the direction a
  reader expects: `browsers!=Chrome` includes events with no browser recorded,
  where plain SQL would silently drop them. Values are always bound, never
  interpolated; only column names are, and those come from a fixed enum. Entry
  and exit pages are refused with a `400` rather than quietly filtered on
  `pathname`, because they are derived from a whole session and have no
  per-event value to match.
- **Raw event export** (`kind=raw`) in CSV or JSON, deliberately excluding
  `visitor_id`: a file of per-event pseudonyms is the artefact this project
  exists to avoid producing.
- **`window_funnel` modes** (`strict`, `strict_order`, `strict_increase`,
  `strict_once`, `allow_reentry`, `timestamp_dedup`) exposed through the funnel
  endpoint, validated against the extension's accepted set.
- **Backward flow analysis** — where visitors came from, not only where they
  went — plus a configurable result limit and a share-of-traffic figure.
- **Configurable visitor-ID salt rotation** (`visitor_salt_rotation_days`). The
  daily default is unchanged, but retention cohorts are impossible under it, and
  `/api/stats/retention` now says so in a `caveat` field rather than presenting
  structural zeros as a finding.
- **Configurable session window** (`session_window_minutes`), replacing the
  hard-coded 30 minutes in every `sessionize` query.
- **Parquet compaction** (`compact_after_files`). A 60-second flush interval
  writes ~1440 files per site per day, and scan cost grew with uptime.
- **A read-connection pool** (`read_connections`), so a slow dashboard query no
  longer blocks event ingestion on the single writer connection.
- **`--healthcheck`**, used by the Docker healthcheck: the image is built
  `FROM scratch` and contains no shell, curl or wget.
- **User-Agent Client Hints** (`Sec-CH-UA`, `-Platform`, `-Mobile`). Chrome
  freezes its legacy UA string, so parsing it alone gives increasingly wrong
  answers. Only the low-entropy hints browsers send by default are read.
- `behavioral_version()` is reported in `/health/detailed`, and the metrics
  endpoint gained dropped-event, cache-eviction, session and read-connection
  series.
- `MALLARD_SITE_IDS` — the allowlist previously required mounting a TOML file.

### Changed

- **Rust 1.94 → 1.98.0** and **edition 2021 → 2024**. The two declarations had
  already drifted — `Cargo.toml` said `1.94.0` while `rust-toolchain.toml`
  pinned `1.94.1` — so a CI step now fails the build when they disagree.
- **DuckDB 1.10501.0 → 1.10505.0**, matching the DuckDB 1.5.5 that the
  `behavioral` community extension pins — the two were a patch series apart.
- Dependencies updated across the board, including three with breaking changes:
  `tower-http` 0.6 → 0.7, `maxminddb` 0.27 → 0.30 and `argon2` 0.5 → 0.6 (whose
  `password-hash` 0.6 generates the salt itself).
- `/api/stats/main` renamed `pages_per_visit` to `views_per_visitor`, which is
  what it always measured, and added a real per-visit figure alongside it.
  Session-derived fields are now nullable.
- `/api/stats/sessions` reports bounce rate alongside the session figures, and
  the whole of `/stats/main` now runs in two queries rather than four full scans
  of `events_all`.
- The query cache evicts least-recently-used entries instead of refusing new
  ones. A full cache previously froze: every subsequent query missed until the
  TTL sweep happened to free space, and a hot key could never displace a cold
  one. It is now applied to every read endpoint rather than two of fourteen
  (`/stats/main` and `/stats/timeseries`), and the concurrency limit likewise
  covers all of them rather than four.
- Bot detection no longer matches the bare substrings `bot`, `fetch` or
  `whatsapp`, which classified the Cubot phone range and WhatsApp's in-app
  browser — real people reading a page — as crawlers.
- The tracking script has a single source. It previously existed as two
  identical files with nothing keeping them in step; it is now compiled in from
  `tracking/script.js` and served from `/mallard.js` (and `/js/script.js`).
- The tracker uses `sendBeacon`, patches `replaceState` as well as `pushState`,
  strips same-origin referrers (which otherwise made each site its own top
  traffic source), and gained opt-out, localhost exclusion, path exclusion,
  outbound-link and file-download tracking.
- The tracker no longer disables itself when `document.currentScript` is null.
  A tag injected dynamically and run asynchronously — what a tag manager does —
  produced no events at all and did not even drain the pre-load queue; it now
  falls back to the first `script[data-domain]` on the page.
- Prerendered pages are no longer lost. The pageview was dropped while the page
  was hidden and never re-fired, so an activated prerender counted as no visit;
  it now fires once on activation.
- The dashboard loads on open, offers a site picker and a custom date range,
  persists its state, supports dark mode, shows chart tooltips, and surfaces
  errors inline instead of through `alert()`. New panels cover realtime,
  revenue, goals and custom properties.
- The binary now uses the library crate instead of re-declaring the module tree,
  so the code is compiled once rather than twice.
- Static dashboard assets are served with cache headers and an ETag.

### Security

- Docker images run as uid 65532 instead of root, carry OCI labels, and set
  `MALLARD_DATA_DIR`. Added a `.dockerignore` — `target/` and `.git/` were
  previously copied into every build context.
- `docker-compose.yml` gained `read_only`, `cap_drop: ALL`,
  `no-new-privileges` and a healthcheck, and the `HEALTHCHECK` now lives in the
  `Dockerfile` too — it was only ever declared in compose, so `docker run` and
  Swarm deployments had none.
- The HTML content security policy gained `frame-ancestors`, `base-uri` and
  `form-action`; JSON and CSV responses are marked `private`.

### Internal

- Added a shared `AppState` test builder. The literal was previously repeated in
  five places, so every new field meant five edits.
- Behavioral-extension tests now assert real results. They were written as
  `if let Ok(x) = query(...) { assert!(...) }`, which passes whether or not the
  extension loads — so a regression in the funnel, retention or session SQL
  could not fail the build. CI runs them with `MALLARD_REQUIRE_BEHAVIORAL=1`,
  which turns a skip into a failure.
- Added CI jobs for the behavioral extension, `actionlint`, JavaScript syntax,
  tracker-copy drift, the tracking script's gzipped size budget, and
  MSRV/toolchain drift, plus Dependabot configuration.
- Clippy runs with `--all-features` in CI, so the `testing`-gated test-support
  modules are linted rather than skipped.
- The MSRV drift check now covers the toolchain literals in the workflows too,
  not just `Cargo.toml` against `rust-toolchain.toml`. There is deliberately no
  separate "build at MSRV" job — `rust-version` and the pinned channel are the
  same version, so it would duplicate the Test job — and that reasoning only
  holds while every pin agrees, which is now enforced rather than assumed.
- The documented local commands set `RUSTDOCFLAGS="-D warnings"` explicitly.
  Both workflows already set it at the workflow level, so CI was catching
  broken intra-doc links all along; a plain local `cargo doc --no-deps` was
  not, which is how one reached a commit here before being caught.
- Query functions take a `QueryScope` rather than four positional `&str`
  arguments that the compiler could not tell apart.
- A test asserts that `mallard-metrics.toml.example` parses, validates, and
  documents every `MALLARD_*` variable the loader reads.
- **Added an end-to-end smoke test** (`scripts/smoke-test.sh`, run by CI). Every
  existing test drives the router in-process; this starts the actual executable
  on a socket with real configuration, a real DuckDB file and real Parquet on
  disk, and exercises every route. Two of the bugs fixed in this release — a
  `500` from `/api/stats/realtime` and an unauthenticated `/api/keys` — were
  invisible to a fully green suite and were both found by running it.
- **Added checks that actually execute the dashboard.** Nothing did before:
  `node --check` validates syntax and says nothing about whether a method
  exists, so a filter button calling a renamed method would have failed only
  when a user clicked it. `scripts/check-dashboard-methods.mjs` catches that
  statically and runs in CI; `scripts/check-dashboard-browser.mjs` drives the
  page in a real browser and fails on any console or page error, and is a local
  script because CI would pay for a browser download on every run.
- Filter tests assert on results, not just on the generated SQL. Two paths get
  their own: `query_core_metrics` swallows a session-query failure into `None`,
  so a test checking only pageview counts would pass while the filtered session
  pass was broken; and retention binds its parameters in a hand-written order
  that a wrong offset would shift silently, producing plausible-looking but
  wrong cohorts rather than an error.
- Time-dependent tests take the instant under test as a parameter rather than
  reading the wall clock, so a run that crosses a minute boundary cannot fail
  and the window edges can be asserted exactly.
- Tests that mutate the process environment recover from a poisoned lock. One
  genuine assertion failure previously poisoned the mutex and made every other
  environment test fail with `PoisonError`, hiding the real failure behind a
  dozen fabricated ones.
- Tests pin down what read-pool connections actually share. `CREATE OR REPLACE
  VIEW` writes to the database catalog, which every connection sees, while
  `LOAD <extension>` arms only the connection that runs it — and comments in
  three files had asserted contradictory things about which was which. Startup
  now loads the extension per reader and defines the view once; a flush or an
  erasure refreshes it on the writer alone, with tests covering both halves.

## [0.1.0] - 2026-04-11

### Added

#### Project Initialization

- Rust project with Axum 0.8, DuckDB 1.4.4 (embedded), Rust 1.93.0 MSRV
- Event ingestion endpoint (`POST /api/event`) with in-memory buffer and periodic flush
- Privacy-safe visitor ID generation (HMAC-SHA256 with daily salt rotation)
- Date-partitioned Parquet storage with ZSTD compression (`site_id` + `date` partitioning)
- DuckDB 25-column events table schema with migration system
- Core metrics queries: unique visitors, total pageviews, bounce rate (sessionize)
- Dimension breakdowns: pages, referrer sources, countries, browsers, OS, devices
- Time-series aggregation with hourly and daily granularity
- Behavioral analytics query builders: funnel (`window_funnel`), retention, sessions (`sessionize`), sequences (`sequence_match`/`sequence_count`), flow (`sequence_next_node`)
- Dashboard SPA (Preact + HTM, embedded in binary via `rust-embed`)
- Tracking script (under 1KB minified JavaScript)
- Health check endpoint (`GET /health`)
- CORS support via `tower-http`
- User-Agent parsing (browser, OS, version detection)
- Referrer source detection (Google, Bing, Twitter, Facebook, Reddit, etc.)
- UTM parameter extraction
- Input validation and sanitization
- CI pipeline (build, test, clippy, fmt, docs, MSRV, bench, security, coverage, docker)
- Criterion.rs benchmark suite for ingestion throughput and Parquet flush
- Dockerfile (multi-stage, `FROM scratch`)
- `docker-compose.yml` with persistent storage

#### Dashboard and Integration Fixes

- Integrated User-Agent parser into ingestion handler (populates browser, OS, version fields)
- Integrated GeoIP stub into ingestion handler (wired for later MaxMind integration)
- Time-series line chart in dashboard (SVG-based, visitors and pageviews)
- All 6 breakdown tables in dashboard (pages, sources, browsers, OS, devices, countries)
- Enhanced tracking script with custom event API (`window.mallard()`) and revenue tracking
- Origin validation enforced on ingestion endpoint

#### Behavioral Analytics

- `GET /api/stats/sessions` endpoint with session metrics (total sessions, avg duration, pages/session)
- `GET /api/stats/funnel` endpoint with safe `page:/path` and `event:name` step format
- `GET /api/stats/retention` endpoint with cohort grid data and `BOOLEAN[]` parsing
- `GET /api/stats/sequences` endpoint with safe pattern generation from conditions
- `GET /api/stats/flow` endpoint with SQL injection prevention for target page
- Dashboard views for all 5 advanced analytics features (sessions, funnel, retention, sequences, flow)
- Graceful degradation for all behavioral queries when the extension is unavailable

#### Production Hardening

- Argon2id password hashing for dashboard authentication
- Session management with 256-bit cryptographic tokens (HttpOnly cookies)
- API key management (create, list, revoke) with SHA-256 hashed storage and `mm_` prefix
- Auth middleware protecting stats, key management, and export routes
- CORS hardening: permissive for ingestion, restrictive for dashboard
- MaxMind GeoLite2 GeoIP reader with graceful fallback
- Bot traffic filtering via User-Agent detection

#### Operational Excellence

- Data retention cleanup (`cleanup_old_partitions()`) with configurable `MALLARD_RETENTION_DAYS`
- Data export API (`GET /api/stats/export`) with CSV and JSON format support
- Graceful shutdown with SIGINT/SIGTERM handling and buffered event flush
- Enhanced health check (`GET /health/detailed`) with JSON system status
- Structured logging with `MALLARD_LOG_FORMAT=json` option
- Configuration template (`mallard-metrics.toml.example`) with all options documented
- Docker build optimization with dependency caching layer

#### Scale and Performance

- TTL-based in-memory query result cache (`query/cache.rs`) for stats and timeseries endpoints
- Per-site token-bucket rate limiter (`ingest/ratelimit.rs`) for ingestion endpoint
- Query benchmarks (core metrics, timeseries, breakdowns) added to Criterion suite
- Prometheus metrics endpoint (`GET /metrics`) with `text/plain; version=0.0.4` format

#### Security and Production Readiness

- Brute-force protection: `LoginAttemptTracker` with per-IP lockout; returns 429 after configurable failures; `MALLARD_MAX_LOGIN_ATTEMPTS` and `MALLARD_LOGIN_LOCKOUT` env vars
- Body size limit: `DefaultBodyLimit::max(65_536)` on ingestion routes; returns 413 on overflow
- OWASP security headers middleware: `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: strict-origin-when-cross-origin`, `Content-Security-Policy` (HTML responses only)
- HTTP timeout: `TimeoutLayer` with 30-second limit prevents Slowloris-style attacks
- CSRF protection: `validate_csrf_origin()` enforced on all session-auth state-mutating routes
- API key scope enforcement: `require_admin_auth` middleware returns 403 for `ReadOnly` keys on key management routes
- `X-API-Key` header supported as alternative to `Authorization: Bearer` for API key auth
- IP audit logging for all auth events (login failures, lockouts, setup, logout, key operations); IPs anonymized before logging
- Prometheus counter `mallard_events_ingested_total` (`AtomicU64`) wired end-to-end through ingest handler
- Config validation at startup: `Config::validate()` exits with error code 1 on invalid settings
- `site_id` validation on all stats endpoints: rejects empty, >256 chars, or non-ASCII-alphanumeric values
- Revoked API key garbage collection runs in a 15-minute background task
- Dashboard export download buttons for CSV and JSON formats
- Funnel chart division-by-zero guard in dashboard JavaScript
- Local JS bundles (`preact.js` + `htm.js`) served via `rust-embed`; CDN dependency eliminated

#### Correctness and Reliability

- Fixed event data loss on flush failure: drained events are restored to the buffer if DuckDB insertion fails
- Fixed blocking I/O in `tokio::spawn` periodic flush: wrapped in `spawn_blocking` to avoid async worker starvation
- Replaced row-by-row `INSERT` with DuckDB Appender API for batch columnar insertion
- Fixed `next_file_path` O(n) stat loop: replaced with single `read_dir` call
- Unified `site_id` validation between ingest and stats endpoints
- Fixed Parquet query gap: `events_all` VIEW unions hot DuckDB table with cold Parquet files
- Fixed `shutdown_timeout_secs` enforcement: flush wrapped in `tokio::time::timeout`
- Fixed `validate_origin` prefix-bypass vulnerability (`example.com.evil.com` no longer matches `example.com`)
- DuckDB disk-based storage: `Connection::open(data_dir/mallard.duckdb)` replaces in-memory; WAL ensures crash durability
- API key store disk persistence: keys survive server restarts via JSON serialization

#### Production Infrastructure

- HSTS header with `max-age`, `includeSubDomains`, and `preload` directives
- `Retry-After` header on all 429 responses
- Cookie `Secure` flag configurable via `MALLARD_SECURE_COOKIES`
- `GET /robots.txt` and `GET /.well-known/security.txt` endpoints
- `X-Request-ID` header with tracing span integration for log correlation
- Concurrent query semaphore (`MALLARD_MAX_CONCURRENT_QUERIES`, default 10)
- `GET /health/ready` readiness probe (queries DuckDB; returns 503 if not ready)
- `CompressionLayer` for gzip/br/zstd response compression
- `Cache-Control: no-store, no-cache` on all JSON API responses
- `Permissions-Policy` header (geolocation, microphone, camera disabled)
- `GET /api/event` pixel tracking (1x1 transparent GIF)
- Auto-generated `MALLARD_SECRET` persisted to `data_dir/.secret`
- `/metrics` optional bearer-token auth via `MALLARD_METRICS_TOKEN`
- Query cache max-entries cap (`MALLARD_CACHE_MAX_ENTRIES`, default 10000)
- Date range validation (max 366 days, end >= start)
- Breakdown limit cap (max 1000)
- `--locked` flag on all CI `cargo` invocations
- `Strict-Transport-Security` preload directive
- `security.txt` with real GitHub advisory contact URL
- SHA-pinned `dtolnay/rust-toolchain` in CI
- `cargo-deny-action` and `cargo-llvm-cov` pre-compiled CI actions

#### GDPR-Friendly Deployment

- `MALLARD_GDPR_MODE` convenience preset for privacy-minimising configuration
- `strip_referrer_query`: strip `?query` and `#fragment` from stored referrers
- `round_timestamps`: round event timestamps to nearest hour
- `suppress_visitor_id`: replace HMAC hash with random UUID per request
- `suppress_browser_version` / `suppress_os_version`: store name only
- `suppress_screen_size`: omit screen width and device type
- `geoip_precision`: configurable ladder (`city`, `region`, `country`, `none`)
- `DELETE /api/gdpr/erase` endpoint for GDPR Art. 17 right-to-erasure requests

#### Documentation

- GitHub Pages documentation site (mdBook) with 13 pages
- `deploy-flyio.md`: complete Fly.io deployment guide
- `deploy-vps.md`: complete VPS deployment guide with LUKS, Caddy, and vps-audit
- `PRIVACY.md`: GDPR/ePrivacy/CCPA analysis with legal citations
- `PERF.md`: benchmark framework and baselines
- `LESSONS.md`: 21 development lessons learned
- `SECURITY.md`: security model, threat model, and vulnerability reporting
- GitHub issue templates (bug report, feature request)
- Pull request template with security checklist
- `CODEOWNERS` file
- `CODE_OF_CONDUCT.md` (Contributor Covenant 2.1)

#### Property-Based and Benchmark Testing

- 7 proptest property tests (visitor_id, ratelimit, cache)
- Criterion benchmark suite restructured: setup moved outside `b.iter()`
- Prometheus counters for flush failures, rate limit rejections, login failures, cache hits/misses
