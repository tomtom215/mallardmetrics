# Importing Data

`mallard-metrics import` loads an export from another analytics product into
Mallard, so a migration does not start from an empty dashboard. It runs offline
(no network access), reads one file, validates every line, and writes the events
through the same storage path the server uses.

It exists for **one** source today, because only that source publishes an
event-level export whose schema is documented. Read
[What other products export](#what-other-products-export) before looking for
your own: for most of them the honest answer is that the data you can download
cannot become events.

```bash
# 1. Check the file. Opens no database, so it works while the server runs.
mallard-metrics import ga4-bigquery --site-id example.com --file events.ndjson --dry-run

# 2. Stop the server, import, start the server.
mallard-metrics import ga4-bigquery --site-id example.com --file events.ndjson
```

## Contents

- [Google Analytics 4 (BigQuery export)](#google-analytics-4-bigquery-export)
- [Command line](#command-line)
- [What other products export](#what-other-products-export)
- [How imported rows behave](#how-imported-rows-behave)
- [Re-running an import](#re-running-an-import)
- [Privacy settings](#privacy-settings)
- [Why a command and not an upload endpoint](#why-a-command-and-not-an-upload-endpoint)
- [If an import is interrupted](#if-an-import-is-interrupted)

## Google Analytics 4 (BigQuery export)

GA4's BigQuery export writes one row per event to daily `events_YYYYMMDD`
tables ([schema](https://support.google.com/analytics/answer/7029846)). Export a
range of those tables to **newline-delimited JSON** (one JSON object per line),
for example with `bq extract --destination_format=NEWLINE_DELIMITED_JSON` or
`EXPORT DATA OPTIONS (format = 'JSON', ...)`, and download the files. For gzip
output, decompress on the way in:

```bash
zcat events_*.json.gz | mallard-metrics import ga4-bigquery --site-id example.com --file -
```

GA4's own reports, its Explorations downloads and the Data API return
aggregates; those are **not** importable (see below). This importer needs the
BigQuery export, which is only available from the day it was linked to the
property: it is not retroactive.

### Field mapping

| Mallard column | From GA4 | Notes |
|---|---|---|
| `timestamp` | `event_timestamp` | UTC microseconds, kept to the microsecond. See [time zones](#time-zones) |
| `event_name` | `event_name` | `page_view` becomes `pageview`; every other name is kept as is |
| `pathname` | `event_params` `page_location` | Path only: scheme, host, query string and fragment are dropped, and the path is normalised as at ingest |
| `hostname` | host of `page_location`, else `device.web_info.hostname` | |
| `referrer`, `referrer_source` | `event_params` `page_referrer` | |
| `utm_*` | query string of `page_location` | Parsed as the tracking endpoint parses them. GA4's `collected_traffic_source` is not used: its `manual_source` also holds referral-derived values, which are not UTM parameters |
| `browser`, `browser_version` | `device.web_info.browser`, `.browser_version` | |
| `os`, `os_version` | `device.operating_system`, `.operating_system_version` | `Macintosh` becomes `macOS` |
| `device_type` | `device.category` | |
| `region`, `city` | `geo.region`, `geo.city` | Only when `geoip_precision` allows |
| `country_code` | — | **Empty.** `geo.country` is a country *name*; Mallard stores an ISO 3166 code and the importer does not guess a mapping |
| `revenue_amount`, `revenue_currency` | `ecommerce.purchase_revenue`, `event_params` `currency` | `purchase` events only, and only when both are present and valid |
| `visitor_id` | `user_pseudo_id` + `ga_session_id` | A keyed hash, never the identifier. See [below](#how-imported-rows-behave) |
| `props`, `screen_size` | — | Empty. GA4's other event parameters are not copied |

Rows that are valid but not imported are counted in the report with their
reason:

- `platform` other than `WEB` (app events have no page).
- Events without a `page_location`.
- `session_start`, `first_visit` and `user_engagement`, which GA4 generates for
  its own bookkeeping and which no visitor did. `--all-events` keeps them.

Never read, and so never stored: `user_id`, `user_pseudo_id` (other than as hash
input), `device.advertising_id`, `device.vendor_id`, `user_properties`, `items`,
campaign click identifiers (`gclid` and the like), and every event parameter not
listed above. The export contains no IP address.

### What is not verified

The field names are those Google documents. Two encodings are inferred rather
than quoted from a document: BigQuery's JSON export writing `INT64` as a quoted
string (a number is accepted too), and `currency` being the parameter name of the
recommended `purchase` event. If your file differs, the dry run says which line
and why.

### Time zones

`event_timestamp` is UTC microseconds, so no zone conversion happens and none
is guessed. The export's `event_date` is in the *property's* time zone and is
ignored. Mallard buckets days in UTC, so a property in another zone sees its
daily totals shift at the edges compared with GA4's own reports. A timestamp
before 2010 (a seconds or milliseconds value by mistake) or more than a day in
the future is rejected.

## Command line

```text
mallard-metrics import <FORMAT> --site-id <ID> --file <PATH|-> [OPTIONS]
```

| Option | Effect |
|---|---|
| `--site-id <ID>` | The site the events are recorded under. Always supplied by you: the source's own property or stream id is never used. Must pass the same rule as ingestion, and be in `site_ids` if you set an allowlist |
| `--file <PATH\|->` | The input; `-` reads standard input |
| `--dry-run` | Validate and count. Opens no database and writes nothing, so it also works while the server is running |
| `--skip-invalid` | Import the valid lines even if some are not. Without it a single invalid line aborts the import before anything is written |
| `--all-events` | Keep the source's bookkeeping events |
| `--config <PATH>` | A configuration file; otherwise `MALLARD_*` variables, exactly as the server reads them |

Invalid lines are reported with their line number, up to twenty of them and then
a count:

```text
line 2: not valid JSON: expected value at line 1 column 1
line 4: event_timestamp 12 is before 2010-01-01; GA4 writes UTC microseconds, not seconds or milliseconds
import: 2 invalid line(s); nothing was written (fix them, or pass --skip-invalid)
```

The report is plain `key: value` lines:

```text
mode: import
lines_read: 1000000
valid_events: 912345
invalid_lines: 0
skipped: 87655
  skipped 87655: user_engagement is GA4 bookkeeping (use --all-events to keep it)
first_event_utc: 2026-01-01 00:00:03.114
last_event_utc: 2026-03-31 23:59:58.002
days_covered: 90
already_present: 0
inserted: 912345
```

Exit status: `0` success, `1` failure (including invalid lines), `2` unusable
command line. Memory stays bounded however large the file is: one line (at most
4 MiB; a longer line is a reported error) and one append chunk are in memory,
and the rest is staged in a DuckDB temporary table that spills to disk under
`duckdb_memory_limit`. Measured figures are in
[Measurements](#measurements).

## What other products export

Researched on 2026-10-21 from each product's own documentation. The question
for each is whether the export has **one row per event** (importable as events)
or only **aggregates** (daily or per-dimension totals). Aggregates cannot be
turned into events without inventing them, so they are not imported: there is no
way to say which of 120 visitors saw which page.

| Product | What the official export contains | Imported? |
|---|---|---|
| **Google Analytics 4** | BigQuery export: one row per event, documented schema ([docs](https://support.google.com/analytics/answer/7029846)). GA4 reports and the Data API: aggregates | **Yes**, the BigQuery export |
| **Plausible** | The dashboard and site-settings export is a ZIP of CSVs of the individual reports; the CSV import format Plausible documents is daily aggregates per dimension (`imported_visitors`: `date, visitors, pageviews, bounces, visits, visit_duration`; `imported_pages`: `date, hostname, page, visits, visitors, pageviews`; and eight more of the same kind). Event-level data exists only as scheduled raw exports to S3-compatible storage on Enterprise plans ([export](https://plausible.io/docs/export-stats), [CSV format](https://plausible.io/docs/csv-import)) | **No.** Aggregates only; the Enterprise raw export has no public schema to build against |
| **Umami** | Cloud export: gzip-compressed CSV of "all collected analytics data" (page views, events, sessions, event data), delivered by email ([docs](https://docs.umami.is/docs/cloud/export-data)). The page names neither the files nor their columns | **No.** It is event-level, but the columns are not documented, and inventing them would produce an importer that works on the author's guess. A file and a column list would be enough to add it |
| **Fathom** | A custom export (pageviews or events, grouped hourly to yearly, by chosen dimensions) and a dashboard download (a ZIP with one CSV per dashboard box) ([docs](https://usefathom.com/docs/features/exporting)). Both are grouped totals | **No.** Aggregates only |
| **Matomo** | Raw visit and action data through the `Live.getLastVisitsDetails` API, per day, paged; or the database directly on self-hosted installs ([FAQ](https://matomo.org/faq/faq_24536/)) | **No.** Event-level, but the response is a nested visit/action structure the official page does not specify field by field, and it is reported to cap the actions returned per visit |

### Aggregate-only exports

If the only thing you can download is daily totals, there is no honest import
into the `events` table. What it would need is a separate *imported aggregates*
table — `(site_id, date, dimension, value, visitors, pageviews, visits,
bounces, visit_duration)` — and report queries that read it for dates before
the first live event and say so on screen. That is a design of its own (every
report would need to know which of its numbers can be summed with aggregate
rows, and unique visitors cannot, since daily visitor counts do not add up to
a period's), and it is not built. Until then, keep the old product's export
as an archive, and expect the Mallard dashboard to start from the day you
installed it.

## How imported rows behave

Imported rows live in the same `events` table and the same Parquet partitions
as live ones, and are marked by their visitor id: **every imported `visitor_id`
starts with `imp:`** (for GA4, `imp:ga4:s:` or `imp:ga4:e:`, then 32 hex
characters). A live id is 64 hex characters, so the two can never be equal and
a live visitor can never be merged into an imported session. `SELECT … WHERE
visitor_id LIKE 'imp:%'` finds every imported row, and the
[Parquet export](exporting-parquet.md) carries an `is_imported` column for the
same purpose (the id itself is never exported).

The id is `HMAC-SHA256` over the source's identifiers, keyed with the
instance's visitor-ID secret (`MALLARD_SECRET` or `data_dir/.secret`), cut to
128 bits. Without that secret it cannot be linked back to the source export.
Changing the secret later changes the ids a re-import would derive; see
[Re-running an import](#re-running-an-import).

What that means for each metric, all pinned by tests:

| Metric | Behaviour for imported rows |
|---|---|
| Pageviews, events, goals, revenue, every breakdown (pages, sources, browsers, UTM, regions, …) | Counted exactly like live rows. A report that mixes both, or covers only imported dates, is identical to the one the same events would give if they had arrived live (tested differentially through `/api/stats/*`) |
| Unique visitors | One visitor **per GA4 session** (`user_pseudo_id` + `ga_session_id`). This counts sessions, not people: a person who visited on three days is three visitors. It is the same trade-off as the default daily salt rotation, and it overstates people compared with GA4's *users* |
| Sessions, bounce rate, duration, pages per session | Reconstructed by the normal sessionisation from the grouped events, so they follow GA4's sessions closely. They are **not GA4's numbers**: Mallard's bounce is "a session with exactly one event, which is a pageview", GA4's is "not engaged" under its own engagement rules; and `user_engagement` and `session_start` are skipped, so GA4's bookkeeping never counts as an interaction here. Expect different figures |
| Retention and cohorts | Imported rows **never form a cohort**. One person's sessions on two days are two unrelated ids, so imported dates show no returning visitors. This is deliberate: linking them would need a stable per-person identifier, which the import will not carry |
| Funnels, sequences, flow | Work within a GA4 session and cannot span sessions, for the same reason |
| Country | Empty (see the mapping table); region and city are filled |
| Raw and Parquet export | Include the rows, never the id |

With `suppress_visitor_id = true` the instance has chosen not to link events at
all, and imports follow it: every imported event gets its own id
(`imp:ga4:e:…`), so visitors equal pageviews and every session is one event,
as for live traffic on that instance. The same happens to a GA4 row that has no
`user_pseudo_id` or `ga_session_id`.

The dashboard does not mark imported days. If a range crosses the import date,
expect a step in visitors and bounce rate there that is a change of definition,
not of traffic.

## Re-running an import

An import is **idempotent**. The key of an imported event is

```text
(site_id, visitor_id, timestamp, event_name, pathname)
```

compared as a multiset: if the store already holds *k* rows with a key and the
file holds *n*, then `max(0, n − k)` are written. Consequences:

- Running the same file twice writes nothing the second time
  (`already_present` equals `valid_events`, `inserted` is 0).
- A later export that overlaps an earlier one writes only the new rows, so you
  can import a month at a time or re-export a range without worrying.
- Two genuinely identical events (a reload within the same second, or within
  the same hour under `round_timestamps`) are both kept the first time and
  neither the second time.
- The same file for a different `--site-id` is a different set of events.
- The ids are derived with the instance's secret. If you change `MALLARD_SECRET`
  between two runs, the second run derives different ids and **will duplicate**.
  Import before rotating the secret, or not at all afterwards.
- The key uses the *stored* values. If privacy settings change between runs
  (turning on `round_timestamps`, say), the keys change too and a re-import
  duplicates. Keep the settings the same for a re-run.

To undo an import, use [erasure](data-management.md#erasure) for the site and
date range, or restore a backup taken before it.

## Privacy settings

An import applies the privacy settings of the instance it runs against, exactly
as the ingest endpoint applies them to live traffic (`gdpr_mode` expands to its
individual settings first):

| Setting | Effect on imported rows |
|---|---|
| `round_timestamps` | Timestamps are rounded down to the hour |
| `strip_referrer_query` | The query string and fragment are cut from `page_referrer` |
| `suppress_browser_version`, `suppress_os_version` | The version columns stay empty |
| `suppress_screen_size` | `device_type` stays empty (as for live events) |
| `geoip_precision` | `city` keeps region and city, `region` keeps region, `country` and `none` keep neither (the country code is always empty) |
| `suppress_visitor_id` | One id per event, as above |

Whatever the settings, an import never stores an IP address, a user id, an
advertising id, a query string from the visited URL (UTM parameters excepted) or
a custom event parameter. Dropped fields are dropped in memory; the temporary
staging table holds only the columns that will be stored.

`round_timestamps` has a cost worth knowing: it is applied, and a rounded
import cannot be un-rounded by turning the setting off later.

## Why a command and not an upload endpoint

DuckDB holds an exclusive lock on the database file while the server runs, so an
importer is either a separate process that runs while the server is **stopped**
(chosen), or a handler inside the running server.

| | Command, server stopped | Admin upload endpoint |
|---|---|---|
| Input size | Any; streamed from disk or a pipe | Bounded by the request timeout (30 s) and a body limit; a multi-GB export needs chunked uploads and resumption |
| Effect on live traffic | None while it runs, but the server is down for the length of the import | The import competes with ingest: it needs the writer connection, and the exclusive tier lock for the flush of every touched partition, which stalls every dashboard query |
| Crash safety | The existing flush-staging and startup recovery, unchanged, plus a rerun | The same, plus partial uploads and a client that retries into a running server |
| Attack surface | None added | A new admin-scope write route that can create partitions for any site |
| Failure handling | Run again | The client must resume or restart |
| Cost | Downtime | Engineering, and a standing endpoint for a once-per-migration job |

The downtime is the price; [Measurements](#measurements) says how long it
is. An operator who cannot stop the instance can run the import against a
copy of the data directory (take a [backup](backup-restore.md) with the server
running, import into the copy, and swap the directories at a convenient
moment), accepting that events recorded in between are not in the copy.

## If an import is interrupted

The import moves rows in two steps: one `INSERT` of everything not yet stored
into the hot table, then the server's own flush of the hot table to Parquet
(write `NNNN.parquet.pending`, delete the hot rows, rename). Every step boundary
has a test that stops the process there and restarts:

| Interrupted | State left | What to do |
|---|---|---|
| While reading the file | Nothing written; the temporary table dies with the process | Run it again |
| After the insert, before the flush | The rows are in the hot table, durable and already visible to reports | Run the same import again (writes nothing, finishes the flush), or just start the server: its next flush moves them |
| After the staged Parquet file is written | A `.pending` file that duplicates the hot rows | Startup recovery discards it; run the import again |
| After the hot rows are deleted | The `.pending` file is the only copy | Startup recovery publishes it. Run the import again to be sure; it writes nothing |

The import runs the same recovery at its start that the server runs at startup,
so running the import is also a way to finish an earlier interrupted one.

## Measurements

A synthetic GA4 file of 200,000 events (190 MB, 20 days), imported with a
**debug build** (`cargo build`) on 4 cores. A release build was not measured, so
treat the times as an upper bound; parsing dominates in a debug build.

| | Result |
|---|---|
| Import into an empty data directory | 48.8 s and 49.8 s (2 runs): about 4,000 events/s |
| Peak memory of the importer | 335 to 348 MB; 301 MB with `MALLARD_DUCKDB_MEMORY_LIMIT=200MB` |
| Parquet written | 6.1 MB (the file is repetitive, so this is optimistic) |
| The same file again (idempotent) | 33.8 s, 0 inserted, 335 MB |
| `--dry-run` | 22.9 s, **22 MB**: constant memory, the file is only streamed |

The 22 MB of the dry run is the streaming claim: one line at a time, whatever
the file size. A real import also stages every row in a DuckDB temporary table
before the merge, so its memory follows the file until DuckDB's memory limit is
reached; set `duckdb_memory_limit` for a very large file. At the debug rate a
million events take about four minutes, which is the downtime to plan for;
measure with `--dry-run` first, which takes about half as long as the import.
