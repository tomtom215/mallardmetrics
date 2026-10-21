# Analytics Stats API

All stats endpoints require authentication: a session cookie, an
`Authorization: Bearer <key>` API key, or an `X-API-Key` header.

Every read endpoint is cached for `cache_ttl_secs` (default 60) except
`/api/stats/realtime` (a cached answer would not be realtime) and
`/api/stats/export` (large, read once). The cache
evicts least-recently-used entries when full, and a GDPR erasure drops that
site's entries immediately.

All endpoints share one concurrency limit (`max_concurrent_queries`, default 10).
A request that finds every slot busy **waits up to 10 seconds** for one; only
then does it get `429` (`code: too_many_requests`, `Retry-After: 1`). A running
statement is interrupted after **20 seconds** and answers `504`
(`code: query_timeout`). Both budgets sit inside the 30-second request timeout
(`408`). Cache hits do not use a slot.

Most reports are also served read-only under `/api/shared/stats/` to a holder of
a [share link](../share-links.md), authenticated by an `X-Share-Token` header
instead of a key. `site_id` is then forced to the link's site and the link's
segment is added to `filters`; exports, `/api/sites` and everything else on this
page are not available that way.

Examples on this page were run against a seeded 0.2.0 binary. Authenticate with
`-H "X-API-Key: mm_..."` or a session cookie.

---

## Common query parameters

| Parameter | Type | Description |
|---|---|---|
| `site_id` | string | Required. The site to query. |
| `period` | string | `day`, `today`, `7d`, `30d`, `90d`, `12mo`. Defaults to `30d`. Anything else is a `400`. |
| `start_date` | string | `YYYY-MM-DD`, **inclusive**. Must be paired with `end_date`; overrides `period`. A lone `start_date` or `end_date` is a `400`. |
| `end_date` | string | `YYYY-MM-DD`, **inclusive**. `end_date` minus `start_date` may be at most 366 days (so up to 367 days are covered); more is a `400`. |
| `limit` | integer | Rows to return, where the endpoint returns a list. Exceeding the endpoint's maximum is a `400`, not a silent clamp. |
| `filters` | string | Narrow the report to a segment. See below. |
| `compare` | string | `previous_period` or `year_over_year`. Read by `/api/stats/main` only. |

Both explicit dates are inclusive, so `start_date=2024-01-01&end_date=2024-01-31`
covers all of January.

Periods are UTC calendar days **including today**: `7d` is seven daily buckets
(today and the six days before), `30d` thirty, `day`/`today` is today so far, and
`12mo` is the twelve months ending today. Sites have no time zone; a day boundary
is a UTC boundary. See [Metric definitions](../metric-definitions.md#periods).

### Segment filters

`filters` narrows every figure a request returns — headline metrics, the time
series, breakdowns, goals, revenue, exports and the behavioral reports alike.

```
filters=browsers==Chrome
filters=countries==DE;devices!=mobile
filters=utm-campaigns==spring,sale-2024
```

Each condition is `dimension==value` or `dimension!=value`, and conditions are
joined by `;`. All conditions must hold, so the set is an AND. A named set of
conditions can be kept on the server as a [saved segment](../share-links.md#saved-segments).

`;` separates conditions and `,` does not: values legitimately contain commas,
as `utm-campaigns==spring,sale-2024` above shows.

**Dimension names are the breakdown slugs** — `browsers`, `countries`,
`utm-sources`, `events` and the rest listed under
[`GET /api/stats/breakdown/{dimension}`](#get-apistatsbreakdowndimension). One
vocabulary, so a row in a breakdown can be turned into a filter without
translating it.

**Matching is exact and case-sensitive.** Values are compared to what is stored,
which is what the corresponding breakdown displays.

**`(unknown)` matches events where the value was not recorded.** A breakdown
renders `NULL` as `(unknown)`, and `filters=browsers==(unknown)` selects exactly
those rows. `!=` is NULL-safe in the direction a reader expects:
`browsers!=Chrome` includes events with no browser at all, because "not Chrome"
plainly covers "no browser recorded" — plain SQL would silently drop them.

**Entry and exit pages cannot be filtered on.** They are derived by looking at a
whole session rather than read from a column, so no per-event predicate
expresses them; asking for one returns `400` rather than quietly filtering on
`pages` and answering a different question.

At most 12 conditions per request, each value at most 512 characters. An unknown
dimension, a missing operator or an empty value is a `400` naming the problem.

A filter **selects sessions, it does not cut them**: a session is kept when at
least one of its events matches every condition, and is then measured whole (see
[Metric definitions](../metric-definitions.md#filters)). Filtered and unfiltered
results are cached separately, and the cache key is
built from the parsed conditions, so `a==1;b==2` and `b==2;a==1` share an entry.

`GET /api/stats/realtime` accepts `filters` but not a date range: its window is
`realtime_window_minutes`, not the request's period. Its per-minute series is
filtered too, so it always agrees with the totals beside it.

### `site_id` validation

Returns `400` unless the value is non-empty, at most 256 characters, and made up
only of ASCII alphanumerics plus `.`, `-`, `_` and `:`. The same rule governs
ingestion, so anything accepted at ingest is queryable. A missing `site_id` is a
plain-text `400` from the query-string parser, not a JSON error.

---

## `GET /api/sites`

Site IDs that have data — from the query view, from the on-disk partitions, and
from the configured `site_ids` allowlist (so a configured site with no data yet
is listed). De-duplicated, **ordered by events in the last 30 days (busiest
first), then by ID**. Events still in the in-memory buffer are not counted until
they are flushed.

`details` carries the figure the order is based on, so a client can show it or
choose a default site. A configured site with no data has `events_30d: 0`.

```json
{
  "sites": ["example.com", "blog.example.com"],
  "details": [
    {"site_id": "example.com", "events_30d": 8412},
    {"site_id": "blog.example.com", "events_30d": 97}
  ]
}
```

---

## `GET /api/stats/main`

```json
{
  "unique_visitors": 1284,
  "total_pageviews": 3910,
  "total_events": 4102,
  "views_per_visitor": 3.05,
  "total_sessions": 1601,
  "bounce_rate": 0.42,
  "avg_visit_duration_secs": 96.4,
  "views_per_visit": 2.44,
  "behavioral_available": true
}
```

| Field | Notes |
|---|---|
| `unique_visitors` | Distinct `visitor_id` values over **all** events in range, custom events included. Over a range longer than one salt rotation this counts visitor-periods, not people — see [Behavioral Analytics](../behavioral-analytics.md#a-prerequisite-that-is-easy-to-miss-visitor-identity). |
| `total_pageviews` | Events named `pageview`. |
| `total_events` | All events, custom ones included. |
| `views_per_visitor` | `total_pageviews / unique_visitors`. Previously called `pages_per_visit`, which is not what it measured. |
| `total_sessions`, `bounce_rate`, `avg_visit_duration_secs`, `views_per_visit` | Need the `behavioral` extension, and exact timestamps. **`null` when unavailable** (extension not loaded, or `gdpr_mode`/`round_timestamps` on), which is meaningfully different from `0`. Definitions: [Metric definitions](../metric-definitions.md). |
| `behavioral_available` | Whether those four could be computed. |
| `sessions_unavailable_reason` | Present only when they could not: `"timestamps_rounded"` (`round_timestamps`/`gdpr_mode`; a privacy setting) or `"extension_missing"` (the `behavioral` extension is not loaded). |
| `comparison` | Present only when `compare` was requested. See below. |

### Comparing against an earlier period

`compare=previous_period` returns the equally long window immediately before
this one; `compare=year_over_year` returns the same dates 365 days earlier. Both
shift the range's exclusive upper bound by the same amount as its lower bound,
so the two windows are always exactly the same length — a 30-day report compared
against 29 days would show a fall in traffic that never happened. A year shift
is 365 days rather than a calendar year for the same reason: a calendar shift
changes the span across a leap year.

```json
{
  "unique_visitors": 1284,
  "…": "…",
  "comparison": {
    "start_date": "2024-05-02",
    "end_date": "2024-05-31",
    "unique_visitors": 1102,
    "total_pageviews": 3301,
    "total_events": 3480,
    "total_sessions": 1390,
    "bounce_rate": 0.45,
    "avg_visit_duration_secs": 88.1
  }
}
```

The dates are inclusive and are returned because "the previous period" is
computed server-side — a reader needs to know which days it was. The two windows
never overlap: the current window's start is the previous window's exclusive
end.

A segment applies to both sides, so a comparison is always like for like. The
comparison window is cached under its own key, so switching between the two
modes reuses whatever either has already computed.

Only `/api/stats/main` reads `compare`. The other endpoints ignore it.

---

## `GET /api/stats/timeseries`

Hourly buckets for ranges of up to two days, daily beyond that. Daily buckets
are labelled `YYYY-MM-DD`, hourly ones `YYYY-MM-DD HH:00`.

```json
[
  {"date": "2024-01-14", "visitors": 0,  "pageviews": 0},
  {"date": "2024-01-15", "visitors": 42, "pageviews": 130}
]
```

Every bucket in range is returned, including empty ones. A chart drawn from a
series with gaps connects the points either side and shows traffic that never
happened.

---

## `GET /api/stats/breakdown/{dimension}`

```json
[
  {"value": "/pricing", "visitors": 210, "pageviews": 260, "events": 271}
]
```

Ordered by visitors, then by value — so equal counts do not reorder between
refreshes.

`entry-pages` and `exit-pages` are **session-level**. Each row also carries
`sessions` (the number of entrances or exits), and its `pageviews` and `events`
count everything in those sessions, not just the entry page:

```json
{"value": "/", "visitors": 2, "pageviews": 6, "events": 7, "sessions": 3}
```

Other dimensions omit `sessions` and count events.

| Dimension | Column |
|---|---|
| `pages` | `pathname` |
| `entry-pages` \* | First pageview of each session |
| `exit-pages` \* | Last pageview of each session |
| `referrers` | `referrer` |
| `sources` | `referrer_source` |
| `countries`, `regions`, `cities` | `country_code`, `region`, `city` |
| `browsers`, `browser-versions` | `browser`, `browser_version` |
| `os`, `os-versions` | `os`, `os_version` |
| `devices`, `screen-sizes` | `device_type`, `screen_size` |
| `utm-sources`, `utm-mediums`, `utm-campaigns`, `utm-contents`, `utm-terms` | The `utm_*` columns |
| `events` | `event_name` |

\* Need the `behavioral` extension and exact timestamps; `503`
(`behavioral_unavailable`) without the extension, or `session_data_unavailable`
under `gdpr_mode`.
They cannot be used in `filters`.

An unknown dimension returns `400` listing the available ones. `limit` defaults
to 10, maximum 1000.

---

## `GET /api/stats/realtime`

```json
{
  "current_visitors": 7,
  "pageviews": 19,
  "window_minutes": 5,
  "top_pages":   [{"value": "/pricing", "visitors": 3}],
  "top_sources": [{"value": "Google",   "visitors": 4}],
  "per_minute":  [2, 5, 3, 4, 3, 2]
}
```

`filters` narrows every figure here, exactly as it does on the other endpoints.

The window is `realtime_window_minutes` (default 5), ending at the current UTC
instant and inclusive at both ends. Events timestamped in the future are outside
it, so a client with a skewed clock cannot inflate "right now".

`per_minute` is gap-filled and ordered oldest first, and holds one entry per
minute boundary in the window — six for a five-minute window, because both ends
are included. Its entries sum to `pageviews`.

---

## `GET /api/stats/goals`

Conversion figures for every event other than `pageview`.

```json
[
  {"name": "signup", "visitors": 64, "events": 71, "conversion_rate": 0.05}
]
```

`conversion_rate` is converting visitors over all visitors in range.

---

## `GET /api/stats/properties` and `GET /api/stats/property-values`

`properties` lists the custom property keys present in range:

```json
["coupon", "plan"]
```

`property-values` breaks one down. `key` is required and must contain only
alphanumerics, `_`, `-` or `.`; `event` optionally restricts it to one event
name.

```
GET /api/stats/property-values?site_id=example.com&key=plan&event=signup
```

```json
[
  {"value": "pro",  "visitors": 41, "events": 44},
  {"value": "free", "visitors": 23, "events": 27}
]
```

---

## `GET /api/stats/revenue`

```json
{
  "by_currency": [
    {
      "currency": "USD",
      "total": 4820.0,
      "transactions": 61,
      "paying_visitors": 58,
      "average_order_value": 79.02
    }
  ],
  "by_event": [{"value": "purchase", "currency": "USD", "total": 4820.0, "transactions": 61}],
  "by_page":  [{"value": "/checkout", "currency": "USD", "total": 4820.0, "transactions": 61}]
}
```

Currencies are always reported separately. There is no exchange-rate source, so
adding 10 USD to 10 EUR would produce a number that means nothing.

---

## Behavioral endpoints

`GET /api/stats/sessions`, `/funnel`, `/retention`, `/sequences` and `/flow` are
documented in full under [Behavioral Analytics](../behavioral-analytics.md).

They return **`503`** (`code: behavioral_unavailable`) with an explanatory
`error` when the `behavioral` extension is not loaded, rather than an empty `200`
that reads as "no data". Sessions, funnel, sequences, flow and the entry/exit
breakdowns return `503` with the different `code: session_data_unavailable`
while `round_timestamps` is on (it is switched on by
`gdpr_mode`), because events inside one hour can no longer be ordered; retention
and every non-session report keep working. `/api/stats/main` then reports its
session fields as `null`.

---

## `GET /api/stats/export`

| Parameter | Default | Description |
|---|---|---|
| `kind` | `daily` | `daily` for one row per day, `raw` for one row per event |
| `format` | `csv` | `csv` or `json` |
| `limit` | `100000` | Raw exports only; maximum 1,000,000 |

A daily export carries each day's **own** top page and source:

```csv
date,visitors,pageviews,top_page,top_source
2024-01-15,42,130,"/pricing","Google"
2024-01-16,38,121,"/blog","Direct"
```

A raw export carries one row per stored event, including rows from an
[import](../importing-data.md). **`visitor_id` is deliberately
excluded**: a file of per-event pseudonyms is precisely the artefact this project
exists to avoid producing.

CSV fields are quoted, internal quotes doubled, and values beginning with `=`,
`+`, `-`, `@`, tab or carriage return are prefixed with an apostrophe so a
spreadsheet does not evaluate them as formulas.

A **raw** export is streamed: the server writes it in chunks as DuckDB produces
the rows, so there is no `Content-Length` (the response is `Transfer-Encoding:
chunked`) and memory use does not grow with `limit`. A 1,000,000-row export grew
the process by about 170 MiB streamed against about 1 GiB buffered in a debug
build (3 runs each). The daily export is small and is not streamed.

Streaming has consequences worth knowing:

- A failure after the first byte cannot change the status code, so the server
  aborts the connection and a client sees a truncated body or a connection reset
  rather than a complete file. Check the size or row count of what you received.
- The export holds the same lock that queries hold while it runs, and a pending
  flush waits for it, so it is capped at 120 s in total. A client that stops
  reading is cut off after the query deadline.
- A raw export of a large range is still work for the server; export in
  date-range slices when you can. The response carries
`Content-Disposition: attachment; filename="mallard-daily-<start>.csv"`
(`mallard-events-<start>.json` for raw JSON).

A day with no events has `top_page` `"(none)"` and `top_source` `"(direct)"`;
a day with events whose referrer is unknown has `"(unknown)"` as its source.
`limit` over 1,000,000 is a `400`. `filters` apply to exports too.

---

## `GET /api/admin/export/parquet`

**Requires admin authentication** (`401` without, `403` for a `ReadOnly` key).
One site and date range as a single Parquet file, for loading into a data
warehouse. `visitor_id` is not in the file. The schema, the consistency
guarantees, the limits and the measurements are on
[Exporting to Parquet](../exporting-parquet.md).

| Parameter | Default | Description |
|---|---|---|
| `site_id` | required | The site |
| `start_date`, `end_date` | `period` | Inclusive `YYYY-MM-DD` dates; both or neither. A range is capped at 366 days |
| `period` | `30d` | Used when no dates are given |

The response is `200` with `Content-Type: application/vnd.apache.parquet`,
`Content-Disposition: attachment; filename="mallard-events-<site>-<start>.parquet"`,
a `Content-Length` (a download that ends short is therefore detectable) and
`X-Mallard-Rows`. A range with more than 1,000,000 events is a `400`, not a
truncated file; a second export while one runs is a `429`; a range that takes
longer than the query deadline is a `504`.

---

## `DELETE /api/gdpr/erase`

**Requires admin authentication** (`401` without, `403` for a `ReadOnly` key).
Permanently deletes a site's events across an inclusive date range, from both the
hot table and the on-disk Parquet partitions, then refreshes the query view and
drops that site's cached results.

All three parameters are required; `end_date` minus `start_date` may be at most
366 days. Order of work: buffered events are flushed first (so they cannot
reappear after the erasure), then both tiers are cleared under the exclusive
tier lock, so no report sees a half-erased state.

```
DELETE /api/gdpr/erase?site_id=example.com&start_date=2024-01-01&end_date=2024-01-31
```

```json
{
  "status": "erased",
  "site_id": "example.com",
  "start_date": "2024-01-01",
  "end_date": "2024-01-31",
  "db_records_deleted": 0,
  "parquet_partitions_deleted": 31
}
```

`db_records_deleted` counts rows removed from the **hot table** only. Because
every flush moves events on to Parquet, it is usually `0` or small; the bulk of
the data is counted by `parquet_partitions_deleted` (one per site-day directory,
which also removes any `.pending`, `.del` and temporary files inside it).

**Partial failure.** If a partition directory cannot be removed, the response is
`500` with `"status": "partial"` and a `failed_dates` array; the other dates were
erased. The data in `failed_dates` is still on disk: fix the cause (usually
permissions or a read-only mount) and repeat the request, which is safe to repeat.

```json
{"status":"partial","site_id":"example.com","start_date":"2024-01-01","end_date":"2024-01-31",
 "db_records_deleted":0,"parquet_partitions_deleted":30,"failed_dates":["2024-01-17"]}
```

Visitor IDs are pseudonymous hashes rather than identities, so a specific
person's rows cannot be singled out. Erasure therefore operates on site and date
range, which is the granularity an operator can act on. Document that limitation
in your privacy notice.
