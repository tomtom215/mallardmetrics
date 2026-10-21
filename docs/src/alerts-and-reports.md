# Webhook Alerts and Scheduled Reports

Mallard can send signed JSON notifications to one webhook: alerts when the
server or a site's traffic misbehaves, and a daily and/or weekly summary per
site. It is **off until you set a webhook URL**, it only ever sends aggregate
numbers (no visitor IDs, no IP addresses), and the destination can be set only
by configuration, never through the HTTP API.

This is the one place Mallard makes outbound connections of its own, apart from
the one-time `behavioral` extension download. Read [Security properties](#security-properties)
before pointing it at anything.

## Quick start

```bash
export MALLARD_ALERTS_WEBHOOK_URL='https://hooks.example.com/mallard'
export MALLARD_ALERTS_WEBHOOK_SECRET="$(openssl rand -hex 32)"
export MALLARD_ALERTS_SITES='example.com'        # default: site_ids
export MALLARD_ALERTS_DAILY_REPORT=true
mallard-metrics --test-webhook                    # sends one test message, exits 0 if accepted
mallard-metrics
```

`--test-webhook` reads the environment only (like `--healthcheck`), applies the
same validation and address checks as the server, and prints only
`scheme://host`.

## What is sent

### Operational alerts (`operational = true`)

Evaluated every `check_interval_secs` (60 s). Each has a stable `id`.

| `id` | Severity | Fires when | Source |
|---|---|---|---|
| `flush_stalled` | critical | no successful flush for more than `flush_stalled_secs` (600 s). A process that has never flushed is measured from its start | `mallard_last_flush_success_timestamp_seconds` |
| `buffer_failing` | critical | the sum of periodic flush failures, threshold-flush failures and dropped events grew inside `failure_window_secs` (300 s) | `mallard_flush_failures_total` + `mallard_buffer_flush_failures_total` + `mallard_events_dropped_total` |
| `behavioral_extension_missing` | warning | the extension is not loaded more than `behavioral_grace_secs` (600 s) after start | `mallard_behavioral_extension` |
| `cold_tier_degraded` | critical | the Parquet cold tier exists but queries do not read it (the `/health/detailed` `degraded` state) | same check as `/health/detailed` |
| `rate_limiter_evictions_spike` | warning | at least `evictions_spike_threshold` (100; 0 disables) rate-limiter evictions inside the window | `mallard_rate_limiter_evictions_total` |

### Traffic alerts

A site's pageviews for one **whole clock hour (UTC)** are compared with the mean
for the *same hour of the same weekday* over the previous
`traffic_baseline_weeks` (4) weeks.

* `traffic_drop:<site>` (warning) fires when the hour is at least
  `traffic_drop_percent` (50) below that mean; `0` disables.
* `traffic_rise:<site>` (info) fires when it is at least `traffic_rise_percent`
  above; default `0` (off).
* **Volume floor.** If the baseline mean is under `traffic_min_pageviews` (50)
  per hour the site is not judged at all, so a site going from 3 to 1 pageviews
  an hour never pages anyone. An existing traffic alert on a site that has fallen
  under the floor is resolved.
* **Missing history is not zero.** A baseline week counts only if the site had
  at least one pageview on that day. At least 2 such weeks are needed; with
  fewer the site is skipped (and an alert that is already firing is left alone).
* **Hysteresis.** An alert clears only when the change is below 80% of its
  threshold, so a value hovering around the threshold does not flap.
* An hour is judged once, after it ended plus a grace of two flush intervals
  (at least two minutes) so its events are on disk. Alerting is therefore up to
  an hour and a few minutes behind the event. Whole hours rather than a sliding
  window, because with `round_timestamps` every event is stamped at the top of
  its hour and a sliding window would see all or nothing.
* The one SQL statement reads only the days involved, through the same query
  runner as the API: it waits for a query slot (`max_concurrent_queries`,
  `query_queue_wait_secs`), holds the tier read lock and is interrupted at
  `query_deadline_secs`. A check that cannot run (busy, timed out) is logged,
  fires and clears nothing, and is retried on the next pass.

Which sites: `alerts.sites`, defaulting to `site_ids`. With neither, only
operational alerts run (a startup advisory says so).

### Cooldown and "resolved"

An alert notifies when it starts firing, again at most every `renotify_secs`
(4 h) while it stays firing, and **once** with `kind: "resolved"` when it clears.
A condition that goes away and returns is a new alert.

**State is in memory.** A restart forgets which alerts were firing, so an alert
that is still true is announced once more after a restart, and a "resolved" for
an alert that cleared while the process was down is never sent. Persisting it
would add a state file whose only benefit is suppressing that one repeat, in
exchange for a new failure mode (a stale or unwritable file), so it is not
persisted.

### Reports

`daily_report` and `weekly_report` send, per site, a summary of the **previous
whole period (UTC)** against the period before it:

* daily: sent after `report_hour_utc` (8) covering yesterday;
* weekly: sent on `weekly_report_day` (monday) after that hour, covering the 7
  days before that date.

Contents: visitors, pageviews, change against the previous period, top 5 pages
and top 5 referrer sources (`(unknown)` is direct traffic). They come from the
dashboard's own query functions through the API's query runner (concurrency cap,
tier lock, deadline). A report that cannot be built is retried every 5 minutes,
five times, then skipped with an error in the log.

**Restarts and reports.** If the process starts after a report's due time, that
period is treated as already sent: a duplicate after every deploy is worse than
a late report. A process that is down at the due time skips that report.

## Payload

`Content-Type: application/json`. One object per request.

```json
{
  "schema": "mallard.notification/v1",
  "delivery_id": "6f1c2d9e-3b0a-4c58-9d0e-5b7f6a2d1c11",
  "kind": "alert",
  "sent_at": "2026-10-21T06:10:03Z",
  "source": "mallard-metrics",
  "version": "0.2.0",
  "text": "[WARNING] example.com: 30 pageviews in the hour from 2026-10-21 05:00 UTC, down 70% from the 100 average of that hour over the previous 4 week(s)",
  "content": "[WARNING] example.com: 30 pageviews ...same sentence...",
  "alert": {
    "id": "traffic_drop:example.com",
    "status": "firing",
    "severity": "warning",
    "category": "analytical",
    "summary": "...same sentence...",
    "site_id": "example.com",
    "since": "2026-10-21T06:10:03Z",
    "notifications": 1,
    "details": {
      "hour_start": "2026-10-21T05:00:00Z", "pageviews": 30, "baseline_mean": 100.0,
      "baseline_weeks_used": 4, "baseline_weeks_requested": 4,
      "change_percent": -70.0, "threshold_percent": 50, "min_pageviews": 50
    }
  }
}
```

* `kind`: `alert`, `resolved` or `report` (`test` from `--test-webhook`).
  `delivery_id` is unique per notification and **identical on retries**, so a
  receiver can de-duplicate.
* `alert` is present for `alert` and `resolved`; `report` for reports:

```json
"report": {
  "period": "daily", "site_id": "example.com",
  "start_date": "2026-10-20", "end_date": "2026-10-20", "timezone": "UTC",
  "visitors": 120, "pageviews": 300,
  "previous": { "visitors": 100, "pageviews": 400 },
  "change_percent": { "visitors": 20.0, "pageviews": -25.0 },
  "top_pages":   [ { "path": "/", "pageviews": 200, "visitors": 90 } ],
  "top_sources": [ { "source": "Google", "pageviews": 60, "visitors": 40 } ]
}
```

  `change_percent` values are `null` when the previous period had none.
* Nothing in a payload identifies a visitor or an address. Page paths are the
  paths your tracker reported; if your URLs contain personal data, the report
  lists them as the dashboard does.

### Slack and Discord

Checked against the products' documentation on 2026-10-21:

| Product | Field | Source |
|---|---|---|
| Slack incoming webhook | `text` (missing `text` is the error `no_text`) | docs.slack.dev, *Sending messages using incoming webhooks* |
| Discord Execute Webhook | `content`, up to 2000 characters; one of `content`, `embeds`, `components`, `file`, `poll` is required | docs.discord.com, *Webhook resource* |

The default `generic` payload carries both `text` and `content` with the same
sentence (cut to 1900 characters). **Neither document says what happens to a
field the product does not know**, so I could not verify that Slack ignores
`content` or Discord ignores `text` (and the other keys). If a receiver rejects
the generic body, set `payload_format`:

| `payload_format` | Body |
|---|---|
| `generic` (default) | the full object above |
| `slack` | `{"text": "..."}` only |
| `discord` | `{"content": "..."}` only |

Slack and Discord webhooks cannot verify the signature (they ignore the
headers), and their URLs are bearer tokens: treat `webhook_url` as a secret.
Discord also offers a Slack-compatible endpoint (`.../slack`); Discord documents
that it does not support Slack's `channel`, `icon_emoji`, `mrkdwn` or
`mrkdwn_in`. Neither product was contacted while writing this page.

## Request signing

Every request carries:

| Header | Value |
|---|---|
| `X-Mallard-Timestamp` | Unix seconds, decimal. **Refreshed on every retry** |
| `X-Mallard-Signature` | `v1=` + lowercase hex of `HMAC-SHA256(secret, timestamp + "." + raw body)` |
| `X-Mallard-Delivery` | the `delivery_id`, constant across retries |
| `X-Mallard-Event` | `kind` |
| `User-Agent` | `mallard-metrics/<version>` |

The secret is `MALLARD_ALERTS_WEBHOOK_SECRET` (at least 16 characters, **environment
only**; a `webhook_secret` key in the TOML file is a startup error). Without it
the signature headers are omitted and a startup advisory says notifications are
unsigned.

**To verify**, on the *raw* body bytes, before parsing the JSON:

1. read the two headers; reject if the timestamp is more than 5 minutes from
   your clock (replay window);
2. compute `HMAC-SHA256(secret, timestamp + "." + body)`;
3. compare with `hmac.compare_digest` (constant time).

```python
import hashlib, hmac, time

def verify(secret: bytes, headers, body: bytes, tolerance: int = 300) -> bool:
    ts = headers["X-Mallard-Timestamp"]
    sig = headers["X-Mallard-Signature"]
    if abs(time.time() - int(ts)) > tolerance:
        return False
    expected = "v1=" + hmac.new(secret, ts.encode() + b"." + body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, sig)
```

This snippet was run against deliveries captured from the real binary; see
[Verified behaviour](#verified-behaviour). The scheme is also pinned in
`alerts::sign` by a test vector produced with
`printf '1700000000.{"a":1}' | openssl dgst -sha256 -hmac k`.

## Delivery

* One worker, one notification at a time, from a **bounded queue of 64**. When it
  is full the *oldest* entry is discarded and counted
  (`mallard_notifications_dropped_total`): a newer message about the same alert
  supersedes it, and a dead receiver cannot grow memory.
* `timeout_secs` (10) bounds each attempt **in total**: DNS, connect (5 s of it
  at most), TLS handshake, request and response head.
* `max_attempts` (4) attempts. Retried: connection errors, timeouts, TLS errors,
  `408`, `429`, `5xx`. Not retried: other `4xx`, redirects, a refused address,
  a malformed response. Wait before retry *n*: `min(30 s, 1 s * 2^(n-1))`, half
  fixed and half random ("equal jitter").
* Success is a `2xx` status line. The response **body is never read**; at most
  16 KiB of response head is read, then the connection is dropped.
* Redirects are **never followed**: a `3xx` is a permanent failure. A receiver
  cannot bounce the request to an internal address.
* A receiver taking more than a few seconds to answer cannot delay anything
  else: the evaluator only enqueues.

## Security properties

This is outbound traffic from a server that may sit inside a private network.

| Property | How | Proved by (in `src/alerts/`) |
|---|---|---|
| Destination comes only from configuration | `[alerts]` / `MALLARD_ALERTS_*`; no route reads or writes it | there is no handler that touches `AlertSettings` |
| `https://` required | `http://` is a startup error unless `allow_http_webhook` | `settings::tests::http_needs_the_flag` |
| No credentials in the URL | `user:pass@host` is a startup error, message does not echo it | `settings::tests::credentials_in_the_url_are_refused_without_echo` |
| Internal addresses refused | every address the name resolves to is checked; **one** bad answer refuses the name. Loopback, private, link-local (incl. `169.254.169.254`), CGNAT (incl. `100.100.100.200`), multicast, unspecified, broadcast/reserved, `168.63.129.16`, IPv6 unique-local (`fc00::/7`, incl. `fd00:ec2::254`), link-local, multicast, and IPv4-mapped/NAT64/6to4/Teredo forms judged by the address inside | `policy::tests::refuses_every_non_public_range`, `tests::literal_internal_addresses_are_refused_without_connecting`, `tests::a_hostname_that_resolves_to_loopback_is_refused`, `tests::localhost_through_the_system_resolver_is_refused`, `tests::one_internal_answer_among_public_ones_refuses_the_name` |
| Startup rejects an obviously internal URL | literal IPs and `localhost` fail validation with a message naming the flag | `settings::tests::internal_literals_and_localhost_need_the_flag` |
| DNS rebinding | one lookup per attempt; the validated `IpAddr` is handed straight to the dialer, which checks it **again** and checks the connected peer; the next attempt resolves and validates afresh | `tests::dns_rebinding_cannot_redirect_a_validated_connection`, `tests::the_real_dialer_refuses_an_internal_address_even_if_the_caller_did_not` |
| No redirects | a `3xx` is a failure | `tests::a_redirect_is_not_followed` |
| Certificates verified | Mozilla roots compiled in (plus `webhook_ca_file`); name checked against the URL host | `tests::https_delivers_to_a_public_address_with_a_verified_certificate`, `tests::an_untrusted_certificate_is_refused`, `tests::a_certificate_for_another_name_is_refused` |
| Strict timeouts | one deadline over the whole attempt | `tests::a_receiver_that_never_answers_times_out` |
| Bounded response | head capped at 16 KiB, body ignored | `tests::an_endless_body_is_ignored_and_an_endless_head_is_refused` |
| Bounded retries and queue | | `tests::it_gives_up_after_the_attempt_limit`, `tests::a_dead_receiver_cannot_grow_the_queue`, `notifier::tests::backoff_doubles_stays_under_the_cap_and_is_jittered` |
| Secret and URL never in logs, metrics or errors | `Secret` and `Target` have redacting `Debug`; logs name `scheme://host`; `DeliveryError` carries categories, never URLs or headers | `tests::neither_the_secret_nor_the_url_reaches_the_logs_or_the_errors` (captures the `tracing` output across success, retry, refusal, redirect, connect and TLS failures) |
| Signature | HMAC-SHA256 over timestamp and body | `tests::a_delivery_is_signed_and_the_signature_verifies`, `sign::tests::matches_openssl` |
| Shutdown never hangs on a receiver | the delivery in flight gets 3 s (`shutdown_timeout` if smaller), then is abandoned; queued items are counted dropped | `tests::shutdown_does_not_wait_for_a_hanging_receiver_and_counts_what_it_discards` |
| Aggregates only | payload fields are listed above | `payload` has no input but site IDs from configuration, counts, paths and sources |

### `allow_private_webhook_targets`

For a receiver on your own network (an internal Mattermost, a collector on the
LAN) set `allow_private_webhook_targets = true`. That **removes the SSRF
protection**: the process will connect to whatever the configured name resolves
to, including cloud metadata endpoints. Only anyone who can already write the
configuration or environment can change the URL, so this is a decision about
who that is. A startup advisory repeats this whenever the flag is on. A private
CA is handled separately, with `webhook_ca_file`.

### Not covered

* **No HTTP proxy support.** `HTTPS_PROXY` is ignored; the connection is direct.
  A deployment whose only route out is a proxy cannot deliver (checks of the
  address policy against a proxy would be meaningless).
* **Egress firewalls still apply.** If your container network has no route out,
  alerts fail and are counted in `mallard_notifications_failed_total`.
* **Cloud metadata services reachable by hostname** are refused because their
  addresses are refused; a metadata service on an unlisted address is not.
* HTTP/1.1 only; no connection reuse (one connection per notification).

## Configuration reference

Table `[alerts]` in the TOML file; every key has an `MALLARD_ALERTS_*`
variable (name upper-cased; `webhook_url` is `MALLARD_ALERTS_WEBHOOK_URL`). The
variable wins over the file. Nothing is validated until a URL is set.

| Key | Default | Validation / effect |
|---|---|---|
| `webhook_url` | unset | `https://host[:port]/path`; no credentials; unset disables everything. **A secret** |
| `MALLARD_ALERTS_WEBHOOK_SECRET` | unset | environment only; at least 16 characters |
| `allow_http_webhook` | `false` | permit `http://` |
| `allow_private_webhook_targets` | `false` | see above |
| `webhook_ca_file` | unset | PEM bundle of extra roots; must exist and contain a certificate |
| `payload_format` | `generic` | `generic`, `slack`, `discord` |
| `timeout_secs` | `10` | 1-60, per attempt, total |
| `max_attempts` | `4` | 1-10 |
| `sites` | `site_ids` | valid site IDs; comma list in the variable |
| `check_interval_secs` | `60` | 10-3600 |
| `renotify_secs` | `14400` | 60-2592000 |
| `operational` | `true` | operational alerts on/off |
| `flush_stalled_secs` | `600` | 60-86400 |
| `failure_window_secs` | `300` | 60-86400 |
| `evictions_spike_threshold` | `100` | 0 disables |
| `behavioral_grace_secs` | `600` | |
| `traffic_drop_percent` | `50` | 0-100, 0 disables |
| `traffic_rise_percent` | `0` | 0-100000, 0 disables |
| `traffic_baseline_weeks` | `4` | 1-12 |
| `traffic_min_pageviews` | `50` | volume floor, per hour |
| `daily_report`, `weekly_report` | `false` | |
| `report_hour_utc` | `8` | 0-23, UTC |
| `weekly_report_day` | `monday` | a weekday name |

A bad combination stops startup with `Configuration error: ...`, for example:

```
Configuration error: alerts.webhook_url uses http://, which sends alerts and report contents unencrypted. Use https://, or set allow_http_webhook = true (MALLARD_ALERTS_ALLOW_HTTP_WEBHOOK) if the path to the receiver is trusted
```

## Metrics

Added to `GET /metrics` (labels have three values, so cardinality is fixed):

| Metric | Type | Meaning |
|---|---|---|
| `mallard_notifications_sent_total{kind}` | counter | accepted by the receiver; `kind` is `alert`, `resolved`, `report` |
| `mallard_notifications_failed_total{kind}` | counter | abandoned after the attempts |
| `mallard_notifications_dropped_total{kind}` | counter | discarded unsent (queue full, shutdown) |
| `mallard_notification_last_success_timestamp_seconds` | gauge | Unix time of the last accepted delivery, 0 before the first |
| `mallard_notification_queue_depth` | gauge | waiting to be delivered |
| `mallard_alerts_active` | gauge | alerts currently firing |

To be told when the notifier itself is broken (a webhook cannot report its own
failure), alert on these in Prometheus; see `deploy/monitoring/`.

## Choosing the HTTP client

The delivery path needs a TLS client whose **connect** can be controlled: the
address that passed the SSRF check must be the address connected to, redirects
must be impossible, and every timeout must be ours. Four options were built as
stand-alone programs (`POST` to a URL, with the project's release profile:
`lto`, `codegen-units = 1`, `strip`, `panic = "abort"`) and measured, then the
chosen one was measured inside Mallard itself.

Hardware: Intel Xeon @ 2.8 GHz, 4 vCPU, 15 GB RAM, Linux 6.18, rustc 1.98.0.
The machine was shared with other builds (load average 1 to 11), so **wall-clock
time is indicative only and CPU time (`user`) is the figure to compare**.

| | tokio only (baseline) | **hand-rolled over `tokio-rustls`** | `ureq` 3 | `hyper-util` + `hyper-rustls` | `reqwest` 0.13 |
|---|---|---|---|---|---|
| Crates added to Mallard's tree (by name) | 0 | **8** | 9 | 11 | 39 |
| Stripped binary, stand-alone | 473,632 B | **1,765,584 B** | 1,933,704 B | 2,097,040 B | 2,561,568 B |
| Over the baseline | | **+1.29 MB** | +1.46 MB | +1.62 MB | +2.09 MB |
| Build, `user` CPU seconds | 19.9 | **53.5** | 53.5 | 73.1 | 106.9 |
| Build, wall seconds | 29 | **51** | 53 | 69 | 97 |
| Licences of the added crates | | MIT, Apache-2.0, ISC, CDLA-Permissive-2.0 (checked from `cargo metadata`) | same set | same set | not checked; adds the ICU and `url` stacks |
| Pin the connect address | | direct: we call `connect(SocketAddr)` | custom `Resolver` (an `unversioned`, unstable API) | custom connector `Service` | `resolve_to_addrs` per client |
| Shutdown | | async: a delivery can be dropped | **blocking**: runs on a blocking thread that cannot be cancelled, and the runtime waits for it | async | async |
| Redirects | | never read | `max_redirects(0)` | not followed by default | `Policy::none()` |
| Response size cap | | ours (16 KiB head, body ignored) | streaming reader | `Limited` body | streaming `chunk()` |

Notes on the table: the size, crate-count and CPU rows are measurements; the
pin, shutdown, redirect and size-cap rows are from each library's documentation
and were **not** exercised, except for the chosen client.

* "Crates added" counts crate names in each stand-alone build's `cargo tree`
  that are not already in Mallard's normal dependency tree. `rustls`, `ring`,
  `webpki-roots` and `ureq` were already in `Cargo.lock` as **build**
  dependencies of `libduckdb-sys`, which is why `deny.toml` already vets them;
  this change moves them into the normal graph. The 8 are `ring`, `rustls`,
  `rustls-pki-types`, `rustls-webpki`, `tokio-rustls`, `untrusted`,
  `webpki-roots` and `zeroize`. `httparse` is already there through `hyper`.
  `rcgen` and its few dependencies (`time`, `yasna`, `pem`) are
  **dev-dependencies**, for a throwaway test certificate.
* `reqwest`'s `rustls` feature selects `aws-lc-rs` (a C/assembly build); it was
  measured with `rustls-no-provider` plus `ring` instead, the fairest
  configuration for it. It reads the OS trust store through
  `rustls-platform-verifier` and brings the ICU and `url` stacks.
* MSRV is no constraint: every candidate needs Rust 1.85 or older; the project
  is on 1.98.
* **CA roots.** The roots are Mozilla's, compiled in (`webpki-roots`), so TLS
  does not depend on what the runtime image contains. The distroless `base`
  image documents that it contains `ca-certificates`
  (`gcr.io/distroless/base`'s README); I did not pull the `cc-debian12` image to
  list its files, and the `Dockerfile` comment that says it has CA roots is not
  something I re-verified. A private CA goes in `webhook_ca_file`. The cost of
  compiled-in roots is that they age with the binary: upgrade Mallard (or use
  `webhook_ca_file`) when a public CA is distrusted.
* `cargo deny check` is **not installed here and could not be obtained**
  (GitHub release downloads are blocked in this sandbox), so it was not run. By
  reading `deny.toml` and the metadata: every new crate's licence expression is
  `MIT`, `Apache-2.0`, `ISC` or `CDLA-Permissive-2.0` (`ring` is
  `Apache-2.0 AND ISC`), all on the allow list; sources are crates.io only; and
  `ring`, `rustls`, `rustls-webpki` and `webpki-roots` were already in the graph
  as build dependencies, so no advisory status changes. `Cargo.lock` also lists
  `rcgen`'s optional `x509-parser` family because Cargo records optional
  dependencies; they are not built.

**Decision: hand-rolled over `tokio-rustls`.** It adds the fewest crates, is no
larger or slower to build than `ureq`, keeps full control of the connect (the
one thing the security design hinges on), is async so shutdown never waits on
it, and what it must do is small: one `POST`, read a status line. The cost is
about 390 lines of HTTP code in `src/alerts/client.rs` that we own: no
chunked/redirect/proxy/HTTP-2 handling, none of which is wanted. `reqwest` would
have cost 39 more crates and about twice the build CPU to avoid writing them.
The weak point of a hand-rolled client is parsing; here it is delegated to
`httparse`, only the response head is parsed, and the tests send malformed and
endless heads.

Inside Mallard (release, `lto = true`):

| | Before | After | Change |
|---|---|---|---|
| Release binary (`cargo build --release --locked`, stripped) | 46,243,176 B | 47,768,136 B | **+1,524,960 B (+1.5 MiB, +3.3%)** |
| Build from scratch (cold target dir, before only) | 29 min 46 s wall, 3,319 CPU s | not repeated | DuckDB's C++ is nearly all of it |
| Rebuild with DuckDB already compiled | 3 min 03 s wall, 318 CPU s (also rebuilt dependencies the shared cache lacked) | 2 min 03 s to 2 min 17 s wall, 118 to 142 CPU s (three runs) | no measurable penalty |

The clean "after" build from scratch was not repeated: the disk was at 95% and a
second 30-minute DuckDB compile into a separate directory did not fit. The
stand-alone programs above isolate the client's own cost: about 34 extra CPU
seconds (53.5 against 19.9), roughly 1% of the 3,319 s a cold build takes.
The before binary built in a different directory measured 46,200,712 B; the
42 KB difference is source paths embedded in the binary.

## Verified behaviour

Run on 2026-10-21 against the real debug binary with `scripts/webhook-e2e.py
<binary> --full` (the quick form is the first four checks, no wait):

* `--test-webhook` to a local receiver delivered one signed request; the
  verification snippet above, copied verbatim into the script, accepted it and
  rejected a body with one byte appended and a wrong secret.
* The same URL without `allow_private_webhook_targets` exited 1 with a message
  naming the setting, echoing neither the URL token nor the secret, and the
  receiver saw no connection. `http://` without `allow_http_webhook` and
  `https://169.254.169.254/...` were refused the same way.
* The running server, with `MALLARD_ALERTS_FLUSH_STALLED_SECS=60` and a one-hour
  flush interval, sent a `flush_stalled` alert from its own background task after
  about a minute; its signature verified; its log contained neither the secret
  nor the URL token; SIGTERM exited 0.
* The Rust suite covers the rest (see the table under
  [Security properties](#security-properties)); `scripts/smoke-test.sh` still
  passes (100 checks) with the feature compiled in but unconfigured.

Not verified: delivery to Slack or Discord themselves (no external network was
used), a real public DNS name over TLS (the TLS tests use a throwaway CA and a
dialer that maps a documentation-range address onto a local listener), IPv6-only
hosts, and behaviour behind an egress proxy (unsupported).
