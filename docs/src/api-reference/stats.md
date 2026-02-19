# Analytics Stats API

All stats endpoints require authentication (session cookie or `Authorization: Bearer` API key).

Query results for `/api/stats/main` and `/api/stats/timeseries` are cached per `(site_id, period)` for `cache_ttl_secs` seconds (default 60).

---

## Common Query Parameters

| Parameter | Type | Description |
|---|---|---|
| `site_id` | string | Required. The site to query. |
| `period` | string | Optional. One of `day`, `today`, `7d`, `30d`, `90d`. Defaults to `30d`. |
| `start_date` | string | Optional. Explicit start date (`YYYY-MM-DD`). Overrides `period`. |
| `end_date` | string | Optional. Explicit end date (`YYYY-MM-DD`, exclusive). Overrides `period`. |

---

## `GET /api/stats/main`

Returns core aggregate metrics.

### Response

```json
{
  "unique_visitors": 1423,
  "total_pageviews": 5812,
  "bounce_rate": 0.42,
  "avg_visit_duration_secs": 0.0,
  "pages_per_visit": 4.08
}
```

| Field | Type | Notes |
|---|---|---|
| `unique_visitors` | integer | Distinct `visitor_id` values in the period. |
| `total_pageviews` | integer | Events where `event_name = 'pageview'`. |
| `bounce_rate` | float | Sessions with exactly one pageview / total sessions. Requires behavioral extension; returns `0.0` if unavailable. |
| `avg_visit_duration_secs` | float | Always `0.0` in this version (requires behavioral extension integration; computed separately via `/api/stats/sessions`). |
| `pages_per_visit` | float | `total_pageviews / unique_visitors`. |

---

## `GET /api/stats/timeseries`

Returns visitors and pageviews bucketed by time.

Granularity is determined automatically from the `period`: `day`/`today` returns hourly buckets; all other periods return daily buckets.

### Response

```json
[
  {"date": "2024-01-15", "visitors": 142, "pageviews": 518},
  {"date": "2024-01-16", "visitors": 167, "pageviews": 603}
]
```

For `period=day` the `date` field includes the hour (e.g. `"2024-01-15 10:00"`).

---

## `GET /api/stats/breakdown/{dimension}`

Returns visitor and pageview counts grouped by a single dimension.

### Dimensions

| Path | Grouped by |
|---|---|
| `/breakdown/pages` | `pathname` |
| `/breakdown/sources` | `referrer_source` |
| `/breakdown/browsers` | `browser` |
| `/breakdown/os` | `os` |
| `/breakdown/devices` | `device_type` |
| `/breakdown/countries` | `country_code` |

### Additional Parameters

| Parameter | Type | Description |
|---|---|---|
| `limit` | integer | Maximum rows to return. Default 10. |

### Response

```json
[
  {"value": "/pricing", "visitors": 312, "pageviews": 489},
  {"value": "/about",   "visitors": 201, "pageviews": 247}
]
```

Unknown/null dimension values are represented as `"(unknown)"`.

---

## `GET /api/stats/sessions`

Returns session-level aggregates using the `sessionize` behavioral function.

Requires the behavioral extension. Returns zeroes if the extension is not loaded.

### Response

```json
{
  "total_sessions": 892,
  "avg_session_duration_secs": 124.7,
  "avg_pages_per_session": 3.2
}
```

---

## `GET /api/stats/funnel`

Returns a conversion funnel where each step is a filter condition.

### Additional Parameters

| Parameter | Type | Description |
|---|---|---|
| `steps` | string | Comma-separated list of steps. Format: `page:/path` or `event:name`. |
| `window` | string | Session window duration. Default `"1 day"`. Must be of the form `N unit` (e.g. `"30 minutes"`, `"2 hours"`). |

### Step Format

| Format | Meaning |
|---|---|
| `page:/pricing` | `pathname = '/pricing'` |
| `event:signup` | `event_name = 'signup'` |

### Example Request

```
GET /api/stats/funnel?site_id=example.com&steps=page:/pricing,event:signup&window=1+hour
```

### Response

```json
[
  {"step": 1, "visitors": 500},
  {"step": 2, "visitors": 120}
]
```

Requires behavioral extension. Returns empty array if unavailable.

---

## `GET /api/stats/retention`

Returns weekly retention cohorts using the `retention` behavioral function.

### Additional Parameters

| Parameter | Type | Description |
|---|---|---|
| `weeks` | integer | Number of cohort weeks to compute. Range: 1–52. Default 4. |

### Response

```json
[
  {
    "cohort_date": "2024-01-08",
    "retained": [true, true, false, true]
  }
]
```

Each `retained` boolean corresponds to one cohort week.

Requires behavioral extension. Returns empty array if unavailable.

---

## `GET /api/stats/sequences`

Returns conversion metrics for a sequence of behavioral steps using `sequence_match`.

### Additional Parameters

| Parameter | Type | Description |
|---|---|---|
| `steps` | string | Comma-separated steps in `page:/path` or `event:name` format. Minimum 2 steps required. |

### Response

```json
{
  "converting_visitors": 89,
  "total_visitors": 500,
  "conversion_rate": 0.178
}
```

Requires behavioral extension. Returns zeroes if unavailable.

---

## `GET /api/stats/flow`

Returns the most common next pages after a given starting page using `sequence_next_node`.

### Additional Parameters

| Parameter | Type | Description |
|---|---|---|
| `page` | string | The target page path to start from (e.g. `/pricing`). |

### Response

```json
[
  {"next_page": "/signup",  "visitors": 234},
  {"next_page": "/contact", "visitors": 89}
]
```

Returns up to 10 results. Requires behavioral extension.

---

## `GET /api/stats/export`

Exports daily aggregated stats as CSV or JSON.

### Additional Parameters

| Parameter | Type | Description |
|---|---|---|
| `format` | string | `csv` (default) or `json`. Any other value returns 400. |

### CSV Response

```csv
date,visitors,pageviews,top_page,top_source
2024-01-15,142,518,/pricing,(direct)
2024-01-16,167,603,/pricing,google
```

CSV fields that might trigger formula injection (start with `=`, `+`, `-`, `@`) are prefixed with a single quote.

`Content-Disposition: attachment; filename="export.csv"` is set so browsers prompt a download.

### JSON Response

```json
[
  {
    "date": "2024-01-15",
    "visitors": 142,
    "pageviews": 518,
    "top_page": "/pricing",
    "top_source": "(direct)"
  }
]
```

`top_page` and `top_source` reflect the single highest-traffic page and referrer source for the entire queried period, not per-day.
