# Metric Definitions

What every number means, with a worked example you can reproduce. These
definitions follow the conventions shared by Plausible and Matomo where one
exists, and are implemented once (`src/query/sessions.rs`) so every endpoint
agrees. If you are comparing against numbers from an earlier release, see
[Upgrading](upgrading.md#definitions-changed-compare-with-care).

## The worked example

All figures below come from this twelve-event dataset for the site
`worked.example` (events are UTC, session window 30 minutes). Visitor IDs are
shown as `v1`..`v4`; in a real instance they are 64-character hashes.

| Visitor | Time (UTC) | Event | Path |
|---|---|---|---|
| v1 | 10-19 09:00 | pageview | `/` |
| v1 | 10-19 09:05 | pageview | `/pricing` |
| v1 | 10-19 09:10 | `signup` | `/pricing` |
| v1 | 10-19 14:00 | pageview | `/pricing` |
| v2 | 10-19 10:00 | pageview | `/blog` |
| v2 | 10-19 10:02 | `click` | `/blog` |
| v3 | 10-19 11:00 | pageview | `/` |
| v3 | 10-19 11:20 | pageview | `/pricing` |
| v3 | 10-19 11:55 | pageview | `/checkout` |
| v4 | 10-19 12:00 | `newsletter` | `/blog` |
| v1 | 10-20 09:00 | pageview | `/` |
| v1 | 10-20 09:03 | pageview | `/pricing` |

(v1 appears on both days with one ID only because the data was inserted
directly; with real ingest each day would carry a different ID unless
`visitor_salt_rotation_days` is raised.)

Seed it yourself with the 30-day demo (`scripts/seed-demo-data.py`) or insert
rows like these into the `events` table of a stopped instance.

## Sessions

A **session** is a maximal run of one visitor's events, of any kind, in which
no two consecutive events are more than the session window apart
(`session_window_minutes`, default 30). Custom events keep a session alive: a
visitor who watches a video between two pageviews was never idle.

A session is **counted** only if it contains at least one pageview. An event
with no pageview behind it (v4's `newsletter`) is not a visit; counting it
would pull every per-visit average toward zero.

Sessions are built from the events **inside the requested range**. A visit
that straddles the range boundary is cut at it.

In the example, for 2026-10-19..20 (`period` or explicit dates):

| Session | Events | Pageviews | Duration |
|---|---|---|---|
| A: v1 09:00-09:10 | 3 | 2 | 600 s |
| B: v1 14:00 (gap of 4 h 50 min) | 1 | 1 | 0 s |
| C: v2 10:00-10:02 | 2 | 1 | 120 s |
| D: v3 11:00-11:20 | 2 | 2 | 1200 s |
| E: v3 11:55 (gap of 35 min) | 1 | 1 | 0 s |
| F: v1 on the 20th | 2 | 2 | 180 s |
| (v4 alone) | 1 | 0 | not counted |

`GET /api/stats/main?site_id=worked.example&period=7d` returns
`total_sessions: 6`, which is A through F.

## Bounce rate

A **bounce** is a counted session whose **only interaction is its pageview**. A
custom event is an interaction, so one pageview plus one click (session C) is not
a bounce. The extras script's background events, `Scroll Depth` and `Engagement`,
are not interactions (the visitor did nothing) and do not end a bounce; they
still count in total events and in visit duration.

Bounces: B and E. `bounce_rate = 2 / 6 = 0.3333`. For the single day
`start_date=end_date=2026-10-19` there are five sessions (F is the 20th), so
`2 / 5 = 0.4` (verified).

## Visit duration

Last event minus first event in the session, in seconds, averaged over counted
sessions. A one-event session lasts 0 s.

`(600 + 0 + 120 + 1200 + 0 + 180) / 6 = 350.0` (`avg_visit_duration_secs`).

## Views per visit and views per visitor

- `views_per_visit` = pageviews per counted session = `9 / 6 = 1.5`.
- `views_per_visitor` = `total_pageviews / unique_visitors` = `9 / 4 = 2.25`.
  (It was called `pages_per_visit` before 0.2.0, which is not what it measured.)

`unique_visitors` counts distinct visitor IDs over **all** events, so v4 counts
(4 visitors) although v4 has no counted session.

## Filters

`filters=dimension==value;dimension!=value` selects **sessions**, not rows: a
session is kept when at least one of its events matches every condition, and is
then measured in full. Filtering rows first would cut a visit into pieces and
make a two-page visit narrowed to one of its pages look like a one-page bounce.

`filters=pages==/pricing` on the example keeps sessions A, B, D and F and
measures them whole:

```json
{"total_sessions":4,"bounce_rate":0.25,"avg_visit_duration_secs":495.0,"views_per_visit":1.75}
```

(`(600 + 0 + 1200 + 180) / 4 = 495`; `(2 + 1 + 2 + 2) / 4 = 1.75`; only B bounces.)

The event counts on the same response (`total_pageviews`, `total_events`,
`unique_visitors`) count only the matching events: here `total_pageviews: 4`.
So under a filter, session figures describe the visits and event figures
describe the matching events, by design.

Matching is exact and case-sensitive. `(unknown)` matches a missing value, and
`!=` includes missing values. Entry and exit pages cannot be filtered on. At
most 12 conditions; values up to 512 characters.

## Entry and exit pages

The **entry page** of a counted session is the path of its first pageview, the
**exit page** that of its last. `GET /api/stats/breakdown/entry-pages` returns
one row per path with `sessions` = entrances, and `pageviews`/`events` totalled
over those sessions:

```json
[{"value":"/","visitors":2,"pageviews":6,"events":7,"sessions":3},
 {"value":"/blog","visitors":1,"pageviews":1,"events":2,"sessions":1},
 {"value":"/checkout","visitors":1,"pageviews":1,"events":1,"sessions":1},
 {"value":"/pricing","visitors":1,"pageviews":1,"events":1,"sessions":1}]
```

`/` is the entry of A, D and F: 3 sessions, 2 + 2 + 2 = 6 pageviews, 3 + 2 + 2 =
7 events. The `sessions` column sums to 6, the total session count.
Under `exit-pages`, `/pricing` has 4 sessions (A, B, D, F).

## Flow

Only pageviews take part, and consecutive repeats of a page are collapsed.
`GET /api/stats/flow?page=/` lists where visitors who viewed `/` went next:

```json
[{"next_page":"/pricing","visitors":2,"share":1.0}]
```

`share` is relative to visitors who reached the page at all (v1 and v3), so
shares need not sum to 1: the remainder left the site there. `direction=backward`
asks where they came from (the field is still named `next_page`).

## Funnels

Each row is the number of visitors who reached **at least** that step within
`window` (default `1 day`, measured from the first step).

`steps=page:/pricing,page:/checkout`:

```json
[{"step":1,"visitors":2,"conversion_rate":1.0,"dropped_off":0},
 {"step":2,"visitors":1,"conversion_rate":0.5,"dropped_off":1}]
```

v1 and v3 viewed `/pricing`; only v3 went on to `/checkout`. With
`window=10 minutes` the second step drops to 0, because v3 took 35 minutes.

**Default mode.** Unless you pass `strict_once` or `strict_increase`, the server
adds `strict_once`: one event advances a funnel by at most one step. The
`behavioral` extension (0.10.0 and later) follows ClickHouse's `windowFunnel`,
where one event that satisfies several steps fills all of them; steps built from
`page:` and `event:` conditions overlap easily, so without this a single
pageview could complete a two-step funnel. `allow_reentry` requires
`strict_order`; alone it is a `400`.

## Sequences

`steps=page:/,page:/pricing` asks how many visitors performed that ordered
pattern at all:

```json
{"converting_visitors":2,"total_visitors":4,"conversion_rate":0.5,"total_matches":3}
```

v1 (sessions A and F) and v3 (D) completed it: 2 visitors, 3 non-overlapping
matches. A visitor contributes once to `converting_visitors` however many times
they match. Unlike a funnel there is no time window.

## Retention

Cohorts are weekly (weeks start on Monday, UTC) by a visitor's **first-seen**
week. `weeks` is 2 to 32.

```json
{"cohorts":[{"cohort_date":"2026-10-19","cohort_size":4,"retained":[4,0],
  "retention_rates":[1.0,0.0],"complete_weeks":0,"partial_first_week":false}],
 "identity_supports_cohorts":false,"caveat":"visitor_salt_rotation_days is 1, ..."}
```

`retained[0]` is the cohort itself. `complete_weeks` is how many leading weeks
have fully elapsed (a low count after that means "not yet", not "did not
return"). `partial_first_week` is true when the cohort's week began before the
report's start, so the cohort is smaller than the week's true new-visitor
count.

**Lookback.** "First seen" is read from `start - visitor_salt_rotation_days`
before the report's start, because an ID cannot outlive its salt. With the
default of 1 day, a visitor who first came earlier than that is counted as new
in the first week. Raising the rotation raises the lookback (and the cost of the
first-seen scan).

## Periods

`period` is relative to today in UTC, **including today**:

| Value | Covers |
|---|---|
| `day`, `today` | today so far |
| `7d` | today and the previous 6 days: 7 daily buckets |
| `30d`, `90d` | 30 or 90 daily buckets |
| `12mo` | the twelve months ending today |

On 2026-10-21, `period=7d` is 2026-10-15 to 2026-10-21 (verified; the comparison
window was 10-08 to 10-14, the same length). In 0.1.x `7d` spanned eight days.
Explicit `start_date` and `end_date` are both inclusive and must be given
together.

`compare=previous_period` shifts the window back by its own length;
`compare=year_over_year` shifts it by 365 days. Either way both windows have
exactly the same number of days.

## Realtime

`/api/stats/realtime` counts events in the last `realtime_window_minutes`
(default 5), up to the current instant and inclusive at both ends, so
`per_minute` has six entries for a five-minute window and they sum to
`pageviews`. Events from the future are outside the window. Because events are
only visible after a flush, "right now" lags by up to `flush_interval_secs`;
lower it (for example to 5) if the realtime panel matters to you.

## When the numbers are `null` or `503`

- `behavioral` extension not loaded: the four session fields on `/main` are
  `null` and session, funnel, sequence, flow and entry/exit endpoints answer
  `503`.
- `round_timestamps` on (it is part of `gdpr_mode`): the same, because events
  inside one hour cannot be ordered. Counts, breakdowns, time series, goals,
  revenue, properties and retention keep working.
