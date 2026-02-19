# Behavioral Analytics

Mallard Metrics integrates the DuckDB [`behavioral` extension](https://github.com/tomtom215/duckdb-behavioral) to provide advanced analytics that go beyond simple counts.

## Prerequisites

The `behavioral` extension is loaded at startup:

```sql
INSTALL behavioral FROM community;
LOAD behavioral;
```

If the extension cannot be loaded (e.g., network unavailable or air-gapped environment), all behavioral endpoints return graceful defaults. Core analytics (visitors, pageviews, breakdowns, timeseries) are unaffected.

The `GET /health/detailed` endpoint does **not** currently report extension status. Check server logs for `"Behavioral extension loaded"` or `"Behavioral extension not available"` at startup.

---

## Session Analytics

**Endpoint:** `GET /api/stats/sessions`

Uses `sessionize(timestamp, INTERVAL '30 minutes')` to group events into sessions per visitor. A new session begins when there is a gap of more than 30 minutes between events from the same visitor.

**Metrics returned:**
- `total_sessions` — Total number of distinct sessions.
- `avg_session_duration_secs` — Mean session duration in seconds.
- `avg_pages_per_session` — Mean pageviews per session.

---

## Funnel Analysis

**Endpoint:** `GET /api/stats/funnel`

Uses `window_funnel(interval, timestamp, step1, step2, ...)` to find visitors who completed a sequence of steps within a time window.

**Example — Pricing to Signup funnel:**

```
GET /api/stats/funnel?site_id=example.com&steps=page:/pricing,event:signup&window=1+day
```

**Step format:**

| Input | SQL condition |
|---|---|
| `page:/pricing` | `pathname = '/pricing'` |
| `event:signup` | `event_name = 'signup'` |

**Response:** Array of `{step, visitors}` showing how many visitors reached each step.

**Notes:**
- Steps must be ordered (each must follow the previous).
- The `window` parameter controls the maximum elapsed time allowed between the first and last step.
- At least 1 step is required; 2+ steps produce a meaningful funnel analysis.

---

## Retention Cohorts

**Endpoint:** `GET /api/stats/retention?weeks=N`

Uses `retention(condition1, condition2, ...)` to compute weekly cohort retention. Each cohort is defined by a visitor's first-seen week. Subsequent weeks show whether they returned.

**Example response (4-week retention):**

```json
[
  {"cohort_date": "2024-01-08", "retained": [true, true, false, true]},
  {"cohort_date": "2024-01-15", "retained": [true, false, true, false]}
]
```

Each boolean in `retained` corresponds to one week: `retained[0]` is always `true` (the cohort week itself), and subsequent booleans indicate whether the visitor was seen in weeks +1, +2, +3, etc.

---

## Sequence Matching

**Endpoint:** `GET /api/stats/sequences`

Uses `sequence_match(pattern, timestamp, cond1, cond2, ...)` to find visitors who performed a specific behavioral pattern. Returns overall conversion metrics.

**Example — Pricing → Signup conversion:**

```
GET /api/stats/sequences?site_id=example.com&steps=page:/pricing,event:signup
```

**Response:**

```json
{
  "converting_visitors": 89,
  "total_visitors": 500,
  "conversion_rate": 0.178
}
```

Minimum 2 steps required.

---

## Flow Analysis

**Endpoint:** `GET /api/stats/flow?page=/pricing`

Uses `sequence_next_node('forward', 'first_match', ...)` to find the most common pages visitors navigate to *after* a given page.

**Response:**

```json
[
  {"next_page": "/signup",  "visitors": 234},
  {"next_page": "/contact", "visitors": 89},
  {"next_page": "/",        "visitors": 67}
]
```

Returns up to 10 next-page destinations ordered by visitor count.

---

## Dashboard Views

The dashboard includes interactive views for all behavioral analytics:

- **Funnel** — Horizontal bar chart with configurable steps.
- **Retention** — Cohort grid table showing Y (returned) / - (not returned) per week.
- **Sequences** — Conversion metrics cards.
- **Flow** — Next-page table with visitor counts.
