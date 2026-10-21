# Behavioral Analytics

Mallard Metrics uses the DuckDB
[`behavioral` extension](https://github.com/tomtom215/duckdb-behavioral) for
analytics that go beyond counting: sessions, funnels, retention cohorts, ordered
sequence matching and navigation flow.

## Prerequisites

The extension is installed and loaded at startup:

```sql
INSTALL behavioral FROM community;
LOAD behavioral;
```

That requires outbound network access on first run (DuckDB fetches
`http://community-extensions.duckdb.org/v<duckdb version>/<platform>/behavioral.duckdb_extension.gz`);
the extension is cached in the extension directory afterwards, and on an
isolated network it can be seeded once with `mallard-metrics --install-extension`
(see [Troubleshooting](troubleshooting.md#running-the-extension-install-step-offline)).

Requirements:

- **Version 0.10.0 or newer.** An older cached build is refused at startup with a
  message telling you to delete `behavioral.duckdb_extension` from the extension
  directory so a current one is downloaded.
- **A glibc platform**: `linux_amd64`, `linux_arm64`, `osx_amd64`, `osx_arm64`.
  The community repository publishes no musl build, so an Alpine/musl binary can
  never load it. The official image is glibc (distroless `cc-debian12`).
- The extension is published per DuckDB version; Mallard 0.2.0 bundles DuckDB 1.5.6.

`GET /health/detailed` reports both
`behavioral_extension_loaded` and `behavioral_version`, and `GET /metrics`
exposes the `mallard_behavioral_extension` gauge.

**When the extension is unavailable, these endpoints return `503`**
(`code: behavioral_unavailable`) **with an explanation**, and `/api/stats/main`
reports its session-derived fields as `null` and says why in
`sessions_unavailable_reason`. The same analyses answer `503` with
`code: session_data_unavailable` while `round_timestamps` is on (`gdpr_mode` turns it on), because events within one
hour can no longer be ordered; retention is the exception and keeps working. That is deliberate: the previous release returned `200` with zeros and
empty arrays, which is indistinguishable from a site that genuinely has no
sessions and no conversions. A missing dependency should look like a missing
dependency.

Core analytics — visitors, pageviews, breakdowns, time series, goals, revenue
and custom properties — do not use the extension and are always available.

---

## A prerequisite that is easy to miss: visitor identity

Every function on this page groups events by `visitor_id`, and that identifier
is an HMAC under a **rotating salt**. Once the salt rotates, the same person is a
different visitor.

With the default `visitor_salt_rotation_days = 1`:

- **Sessions, funnels, sequences and flow work normally** as long as the
  behaviour happens within a single UTC day, which covers almost all of it. The
  one artefact is that a visit spanning midnight is recorded as two visits.
- **Weekly retention cohorts cannot work at all.** A visitor returning a week
  later carries an unrelated identifier, so every week past the first is
  structurally zero no matter how loyal the audience is.

`GET /api/stats/retention` reports this directly: the response carries
`identity_supports_cohorts`, and when it is `false` a `caveat` string explaining
what to change, with the configured `visitor_salt_rotation_days` beside it so a
client can state the real value without parsing the text. Retention over *N* weeks needs
`visitor_salt_rotation_days >= (N - 1) * 7`.

Raising the rotation is a privacy decision, not a tuning knob — see
[Security & Privacy](security.md) and `PRIVACY.md` for the trade-off.

---

## Sessions

**Endpoint:** `GET /api/stats/sessions`

`sessionize(timestamp, INTERVAL '<session_window_minutes> minutes')` groups a
visitor's events (of every kind) into sessions; a gap longer than the window
starts a new one. The window defaults to 30 minutes (`session_window_minutes`).
Only sessions with at least one pageview are counted.

```json
{"total_sessions":6,"avg_session_duration_secs":350.0,"avg_pages_per_session":1.5,"bounce_rate":0.3333333333333333}
```

| Field | Description |
|---|---|
| `total_sessions` | Counted sessions in range |
| `avg_session_duration_secs` | Mean of last event minus first event |
| `avg_pages_per_session` | Mean pageviews per counted session |
| `bounce_rate` | Fraction of counted sessions that are exactly one event, a pageview. A custom event ends the bounce. 0.0–1.0 |

Exact definitions, with a worked example, are on
[Metric Definitions](metric-definitions.md). Filters select whole sessions.

The same figures appear on `/api/stats/main`, computed in the same pass rather
than a second one.

---

## Funnels

**Endpoint:** `GET /api/stats/funnel`

```
GET /api/stats/funnel?site_id=example.com
    &steps=page:/,page:/pricing,event:signup
    &window=1 day
```

| Parameter | Default | Description |
|---|---|---|
| `steps` | (required) | 2–32 comma-separated steps |
| `window` | `1 day` | Maximum elapsed time from first step to last. `<n> <unit>` with unit `second(s)` (up to 86,400), `minute(s)` (1,440), `hour(s)` (720), `day(s)` (365) or `week(s)` (52); anything else is a `400` |
| `modes` | (none) | Comma-separated ordering modes, below |

Step format:

| Input | SQL condition |
|---|---|
| `page:/pricing` | `pathname = '/pricing'` |
| `event:signup` | `event_name = 'signup'` |

### The report is cumulative

Each row is the number of visitors who reached **at least** that step:

```json
[
  {"step": 1, "visitors": 4, "conversion_rate": 1.0,  "dropped_off": 0},
  {"step": 2, "visitors": 2, "conversion_rate": 0.5,  "dropped_off": 2},
  {"step": 3, "visitors": 1, "conversion_rate": 0.25, "dropped_off": 1}
]
```

`conversion_rate` is relative to step 1; `dropped_off` is the loss since the
previous step. A row is returned for every step even when its count is zero, so
the report always describes the funnel's shape.

> `window_funnel` itself returns the *furthest* step each visitor reached.
> Grouping by that value directly — which an earlier release did — produces
> "visitors who stopped at exactly step N", which is a different and much less
> useful report: a visitor who converted all the way through is not counted at
> step 1 at all.

### Ordering modes

Any combination, comma-separated:

| Mode | Effect |
|---|---|
| `strict` | A repeat of the previously-matched condition breaks the chain |
| `strict_deduplication` | Alias for `strict` |
| `strict_order` | No event matching an earlier condition may appear between matched steps |
| `strict_increase` | Timestamps must strictly increase between steps |
| `strict_once` | One event may advance the funnel by at most one step |
| `allow_reentry` | A repeat of the entry condition restarts the funnel |
| `timestamp_dedup` | Events sharing the previous step's timestamp are skipped |

An unrecognised mode is a `400`, not a `500`. So is `allow_reentry` without
`strict_order` (the extension refuses that combination); the `400` message in
that case reads "Unknown funnel mode", which is misleading.

**Default.** The server adds `strict_once` to whatever you pass unless you passed
`strict_once` or `strict_increase`, so one event fills at most one step. Without
it the `behavioral` extension (0.10.0+, which follows ClickHouse `windowFunnel`)
lets one event satisfy several steps. See
[Metric Definitions](metric-definitions.md#funnels).

---

## Retention cohorts

**Endpoint:** `GET /api/stats/retention?weeks=N`

Cohorts are defined by a visitor's first-seen week. `weeks` must be between
**2 and 32** — the extension's `retention()` accepts 2 to 32 conditions.

```json
{
  "cohorts": [
    {
      "cohort_date": "2024-01-01",
      "cohort_size": 4,
      "retained": [4, 2, 1, 0],
      "retention_rates": [1.0, 0.5, 0.25, 0.0],
      "complete_weeks": 2,
      "partial_first_week": false
    }
  ],
  "identity_supports_cohorts": false,
  "caveat": "visitor_salt_rotation_days is 1, so visitor identities do not survive…",
  "visitor_salt_rotation_days": 1
}
```

`retained[i]` is how many of the cohort were seen in week *i*;
`retention_rates[i]` is that as a fraction of `cohort_size`. Index 0 is the
cohort week itself.

`complete_weeks` is the number of leading weeks that have fully elapsed (later
zeros mean "not yet"), and `partial_first_week` marks a cohort whose first week
began before the report's start. Weeks start on Monday (UTC).

Only cohorts that formed inside the queried range are reported. "First seen" is
looked up from `visitor_salt_rotation_days` before the range start, because an
ID cannot outlive its salt; the scan is bounded rather than reading the site's
entire history.

---

## Sequence matching

**Endpoint:** `GET /api/stats/sequences`

```
GET /api/stats/sequences?site_id=example.com&steps=page:/pricing,event:signup
```

```json
{
  "converting_visitors": 89,
  "total_visitors": 500,
  "conversion_rate": 0.178,
  "total_matches": 112
}
```

`converting_visitors` counts people; `total_matches` counts non-overlapping
completions, so a visitor who completed the sequence three times contributes 1
and 3 respectively. Steps use the same format as funnels; 2 to 32 are accepted.

The difference from a funnel: a funnel measures how far people get within a time
window, a sequence asks whether an exact ordered pattern occurred at all.

---

## Flow

**Endpoint:** `GET /api/stats/flow?page=/pricing`

| Parameter | Default | Description |
|---|---|---|
| `page` | (required) | The page to analyse around |
| `direction` | `forward` | `forward` for where visitors went next, `backward` for where they came from |
| `limit` | `10` | Destinations to return, up to 100 |

```json
[
  {"next_page": "/signup",  "visitors": 234, "share": 0.47},
  {"next_page": "/contact", "visitors": 89,  "share": 0.18}
]
```

`share` is relative to the visitors who reached `page` at all, so the values do
not need to sum to 1 — the remainder left the site there, which is exactly what
makes a high-exit page visible.

---

## Dashboard

The dashboard renders all of the above:

- **Overview** — headline metrics, with session-derived figures marked when the
  extension is unavailable rather than shown as zero.
- **Funnel** — a cumulative bar chart with drop-off, plus inputs for steps and modes.
- **Retention** — a cohort grid shaded by retention rate, with the identity
  caveat shown inline when it applies.
- **Sequence** — converting visitors, conversion rate and total completions.
- **Flow** — next or previous pages with visitor counts and share.
