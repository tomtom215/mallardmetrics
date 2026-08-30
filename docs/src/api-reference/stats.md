# Analytics Stats API

All stats endpoints require authentication: a session cookie, an
`Authorization: Bearer <key>` API key, or an `X-API-Key` header.

Every read endpoint is cached for `cache_ttl_secs` (default 60) except
`/api/stats/realtime`, where a cached answer would not be realtime. The cache
evicts least-recently-used entries when full, and a GDPR erasure drops that
site's entries immediately.

All endpoints share one concurrency limit (`max_concurrent_queries`, default 10)
and return `429` with a `Retry-After` header when it is reached.

---

## Common query parameters

| Parameter | Type | Description |
|---|---|---|
| `site_id` | string | Required. The site to query. |
| `period` | string | `day`, `today`, `7d`, `30d`, `90d`, `12mo`. Defaults to `30d`. |
| `start_date` | string | `YYYY-MM-DD`, **inclusive**. Must be paired with `end_date`; overrides `period`. |
| `end_date` | string | `YYYY-MM-DD`, **inclusive**. Range may span at most 366 days. |
| `limit` | integer | Rows to return, where the endpoint returns a list. Exceeding the endpoint's maximum is a `400`, not a silent clamp. |
| `filters` | string | Narrow the report to a segment. See below. |

Both explicit dates are inclusive, so `start_date=2024-01-01&end_date=2024-01-31`
covers all of January.

### Segment filters

`filters` narrows every figure a request returns — headline metrics, the time
series, breakdowns, goals, revenue, exports and the behavioral reports alike.

```
filters=browsers==Chrome
filters=countries==DE;devices!=mobile
filters=utm-campaigns==spring,sale-2024
```

Each condition is `dimension==value` or `dimension!=value`, and conditions are
joined by `;`. All conditions must hold, so the set is an AND.

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

Filtered and unfiltered results are cached separately, and the cache key is
built from the parsed conditions, so `a==1;b==2` and `b==2;a==1` share an entry.

`GET /api/stats/realtime` does not accept `filters`: it reports a live snapshot
rather than a report over a range.

### `site_id` validation

Returns `400` unless the value is non-empty, at most 256 characters, and made up
only of ASCII alphanumerics plus `.`, `-`, `_` and `:`. The same rule governs
ingestion, so anything accepted at ingest is queryable.

---

## `GET /api/sites`

Site IDs that have data — from the query view, from the on-disk partitions, and
from the configured `site_ids` allowlist.

```json
{"sites": ["blog.example.com", "example.com"]}
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
| `unique_visitors` | Distinct `visitor_id` values. Over a range longer than one salt rotation this counts visitor-periods, not people — see [Behavioral Analytics](../behavioral-analytics.md#a-prerequisite-that-is-easy-to-miss-visitor-identity). |
| `total_pageviews` | Events named `pageview`. |
| `total_events` | All events, custom ones included. |
| `views_per_visitor` | `total_pageviews / unique_visitors`. Previously called `pages_per_visit`, which is not what it measured. |
| `total_sessions`, `bounce_rate`, `avg_visit_duration_secs`, `views_per_visit` | Need the `behavioral` extension. **`null` when unavailable**, which is meaningfully different from `0`. |
| `behavioral_available` | Whether those four could be computed. |

---

## `GET /api/stats/timeseries`

Hourly buckets for ranges up to two days, daily beyond that.

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

\* Needs the `behavioral` extension; returns `503` without it.

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

They return **`503`** with an explanatory `error` when the `behavioral`
extension is not loaded, rather than an empty `200` that reads as "no data".

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

A raw export carries one row per stored event. **`visitor_id` is deliberately
excluded**: a file of per-event pseudonyms is precisely the artefact this project
exists to avoid producing.

CSV fields are quoted, internal quotes doubled, and values beginning with `=`,
`+`, `-`, `@`, tab or carriage return are prefixed with an apostrophe so a
spreadsheet does not evaluate them as formulas.

The response is built in memory rather than streamed, so `limit` is also a
memory bound: the 100,000-row default is a few tens of megabytes, and the
1,000,000-row ceiling a few hundred. Export in date-range slices rather than
raising the limit on a memory-constrained host.

---

## `DELETE /api/gdpr/erase`

**Requires admin authentication.** Permanently deletes a site's events across an
inclusive date range, from both the hot table and the on-disk Parquet
partitions, then refreshes the query view and drops that site's cached results.

```
DELETE /api/gdpr/erase?site_id=example.com&start_date=2024-01-01&end_date=2024-01-31
```

```json
{
  "status": "erased",
  "site_id": "example.com",
  "start_date": "2024-01-01",
  "end_date": "2024-01-31",
  "db_records_deleted": 1204,
  "parquet_partitions_deleted": 31
}
```

Visitor IDs are pseudonymous hashes rather than identities, so a specific
person's rows cannot be singled out. Erasure therefore operates on site and date
range, which is the granularity an operator can act on. Document that limitation
in your privacy notice.
