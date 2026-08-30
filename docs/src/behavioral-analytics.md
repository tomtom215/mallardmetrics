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

That requires outbound network access on first run; the extension is cached
afterwards. `GET /health/detailed` reports both
`behavioral_extension_loaded` and `behavioral_version`, and `GET /metrics`
exposes the `mallard_behavioral_extension` gauge.

**When the extension is unavailable, these endpoints return `503` with an
explanation**, and `/api/stats/main` reports its session-derived fields as
`null`. That is deliberate: the previous release returned `200` with zeros and
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
what to change. Retention over *N* weeks needs
`visitor_salt_rotation_days >= (N - 1) * 7`.

Raising the rotation is a privacy decision, not a tuning knob — see
[Security & Privacy](security.md) and `PRIVACY.md` for the trade-off.

---

## Sessions

**Endpoint:** `GET /api/stats/sessions`

`sessionize(timestamp, INTERVAL '<session_window_minutes> minutes')` groups a
visitor's events into sessions; a gap longer than the window starts a new one.
The window defaults to 30 minutes and is configurable through
`session_window_minutes`.

| Field | Description |
|---|---|
| `total_sessions` | Distinct sessions in range |
| `avg_session_duration_secs` | Mean time from a session's first event to its last |
| `avg_pages_per_session` | Mean pageviews per session |
| `bounce_rate` | Fraction of sessions with exactly one pageview, 0.0–1.0 |

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
| `window` | `1 day` | Maximum elapsed time from first step to last |
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

An unrecognised mode is a `400`, not a `500`.

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
      "retention_rates": [1.0, 0.5, 0.25, 0.0]
    }
  ],
  "identity_supports_cohorts": false,
  "caveat": "visitor_salt_rotation_days is 1, so visitor identities do not survive…"
}
```

`retained[i]` is how many of the cohort were seen in week *i*;
`retention_rates[i]` is that as a fraction of `cohort_size`. Index 0 is the
cohort week itself.

Only cohorts that formed inside the queried range are reported, and the
first-seen scan is bounded by the range — an earlier release scanned the site's
entire history on every request.

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
