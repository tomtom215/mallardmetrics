# Exporting to Parquet

`GET /api/admin/export/parquet` returns one site and one date range as a single
Parquet file, for loading into a warehouse or a notebook. It is an
[admin-scope](api-reference/auth.md) endpoint: a read-only API key gets `403`.

```bash
curl -fsS -H "Authorization: Bearer mm_..." \
  "http://localhost:8000/api/admin/export/parquet?site_id=example.com&start_date=2026-03-01&end_date=2026-03-31" \
  -o example.com-2026-03.parquet
```

| Parameter | Description |
|---|---|
| `site_id` | The site (required) |
| `start_date`, `end_date` | Inclusive `YYYY-MM-DD` dates, both or neither; at most 366 days. Days are UTC |
| `period` | `7d`, `30d`, … when no dates are given (default `30d`) |

Response: `200`, `Content-Type: application/vnd.apache.parquet`,
`Content-Disposition: attachment; filename="mallard-events-<site>-<start>.parquet"`,
`Content-Length`, `Cache-Control: no-store`, and `X-Mallard-Rows` with the row
count. A download that ends before `Content-Length` bytes is incomplete; the
server aborts the connection rather than finish a short file.

| Status | Meaning |
|---|---|
| `400` | Invalid site or dates, or more than **1,000,000** events in the range. The file is never truncated: a warehouse load that silently lacks its tail is worse than an error. Narrow the range |
| `401`, `403` | Not authenticated; read-only key |
| `429` | Another Parquet export is running (one at a time), or no query slot freed up in time |
| `504` | Writing the file took longer than `query_deadline_secs` (default 20). Narrow the range |

## Schema

One row per stored event, in **no guaranteed order** (days are read oldest
first, but rows are not sorted: sorting made the server's peak memory about
four times larger for no gain a warehouse needs). Compression is ZSTD. The same list is in `src/query/parquet_export.rs`, and a
test checks it against the file DuckDB actually writes.

| Column | Type | Nullable | Notes |
|---|---|---|---|
| `site_id` | `VARCHAR` | no | The same value in every row; present so files from several sites can be unioned |
| `timestamp` | `TIMESTAMP` (microseconds) | no | **UTC**, no zone attached (Parquet `isAdjustedToUTC = false`). With `round_timestamps` it is the hour |
| `event_name` | `VARCHAR` | no | `pageview` or a custom name |
| `pathname` | `VARCHAR` | no | |
| `hostname` | `VARCHAR` | yes | |
| `referrer` | `VARCHAR` | yes | |
| `referrer_source` | `VARCHAR` | yes | |
| `utm_source`, `utm_medium`, `utm_campaign`, `utm_content`, `utm_term` | `VARCHAR` | yes | |
| `browser`, `browser_version`, `os`, `os_version` | `VARCHAR` | yes | |
| `device_type`, `screen_size` | `VARCHAR` | yes | `screen_size` is the viewport width in CSS pixels, as text |
| `country_code` | `VARCHAR` | yes | ISO 3166-1 alpha-2 |
| `region`, `city` | `VARCHAR` | yes | |
| `props` | `VARCHAR` | yes | JSON object text; query it with your warehouse's JSON functions |
| `revenue_amount` | `DECIMAL(12,2)` | yes | |
| `revenue_currency` | `VARCHAR` | yes | ISO 4217 |
| `is_imported` | `BOOLEAN` | no | `true` for rows loaded by [`mallard-metrics import`](importing-data.md) |

A column that is empty for every event in the range is still present, with its
type.

### Why `visitor_id` is not in the file

It is the same rule as the [raw export](api-reference/stats.md#get-apistatsexport).
`visitor_id` is a pseudonymous identifier: a keyed hash of site, IP address and
User-Agent that changes when the salt rotates. Inside Mallard that is what makes
"unique visitors" work without storing who anyone is. In a warehouse, a column
of per-event pseudonyms can be joined to anything else the warehouse holds that
shares the inputs, which is the artefact this project exists not to produce.
Anything derived from it that you might want (sessions, visitor counts) is
better computed by Mallard and read from `/api/stats/*`.
`is_imported` is the one thing taken from it, and it identifies nobody.

## How the file is made, and why

DuckDB writes the file with `COPY ... TO`, and it can write only to a file:
Parquet puts its footer last, so the encoder needs the whole row set before
the file is valid, and DuckDB has no sink for a socket. So the choice was never
"stream the file or hold it in memory" but where the file waits:

1. **Write to `<data_dir>/exports`, return the file in the same response**
   (chosen). The handler writes `export-<uuid>.parquet.tmp`, streams it to the
   client in 64 KiB chunks and deletes it when the download ends, the client
   leaves, or 120 seconds pass.
2. Write to `<data_dir>/exports` and return a name for a second request. This
   needs a lifecycle for abandoned files and a second authenticated route, and
   would only be needed if writing the file took longer than the request can
   wait. The measurements below say it does not at the supported size.

The file name is generated, never taken from the request, and the directory is
created under the data directory with owner-only permissions, so nothing is
written outside it. Files older than an hour that a crash left behind are
removed by the next export. The directory is not part of an
[online backup](backup-restore.md).

### Consistency

- The `COPY` runs inside the same read path the analytics queries use: it holds
  the **shared tier lock** for exactly as long as the `COPY` runs. A flush,
  compaction or erasure cannot change the file list under it, so every row is
  in the file once, from the hot table or Parquet and never both.
- The Parquet files read are only those of the requested site and days (the same
  scoped file list the reports use); a request does not scan other sites.
- The lock is released before the download starts. The streaming raw export
  holds it for the whole download (up to two minutes), so this endpoint stalls
  a pending flush for less time than that export does.
- The hot table is read in the statement's own snapshot, so events ingested
  while the file is written are either in it or not, never half.

### Limits

- 1,000,000 rows per file; a larger range is refused.
- One export at a time per server, so at most one file is on disk.
- Subject to the query deadline and concurrency cap like every analytics query.
- It needs free disk for one file under `data_dir` (about
  17 bytes per row on the benchmark data; see [Measurements](#measurements)).

## Measurements

Dataset: 1,100,000 synthetic events for one site over 30 days, flushed to
Parquet (48 MB on disk, 30 partitions), then the first 26 days exported:
**976,696 rows**. Debug build (`cargo build`) on 4 cores, 16 GiB RAM; **a release
build was not measured**, so treat the times as an upper bound. The data has
less variety than real traffic (no long User-Agent-derived strings, few distinct
referrers), so the file size in particular is optimistic.

| | Result |
|---|---|
| Time to write the file (`COPY`, tier lock held) | 9.5 to 9.9 s (3 runs) |
| Request, end to end through the real binary, loopback | 10.8 s |
| File size | 16.2 MB, about 17 bytes per row |
| Peak disk used by the export | the file itself: 15.7 MB observed on disk, no temporary spill |
| Server memory, high-water mark | 208 MB, from 84 MB idle: about +124 MB |
| Process memory in the unit harness | 178 to 184 MB (from 38 MB), 3 runs |
| Same export with `ORDER BY timestamp` | 795 to 824 MB for the same time, and 201 MB of spill under a 512 MB DuckDB limit |
| 1,100,000 rows (over the cap) | refused after 11.4 and 11.7 s (the cost of finding out; the partial file, 16 MB, is deleted) |
| 112,696 rows (3 days) | 1.3 to 1.4 s, memory high-water mark 140 to 142 MB |

What these decided:

- **Write a file, then stream it.** Writing took about 10 s for a million rows,
  inside the 20 s default query deadline, so a second "come back for it"
  request is unnecessary at the supported size. Disk is one file; memory is
  DuckDB's working set, not the file.
- **No sort.** It would have multiplied peak memory by about four.
- **Hold the tier lock for the write only.** The ~10 s above is the whole time
  a pending flush can be made to wait, against up to 120 s for a streamed raw
  export of the same size.
- **The cap is 1,000,000 rows.** At that size the write is within a factor of
  two of the default deadline on a debug build. A deployment that sets
  `query_deadline_secs` lower will see `504` on large ranges, which is the
  deadline doing its job; export in slices.

`peak_disk` was sampled every 20 ms from a separate thread, so it is a lower
bound on the true instantaneous peak.

## Verification

The file was read back with DuckDB only: the row count equals `X-Mallard-Rows`,
the timestamp range is the requested one, and DuckDB reports the column as
`TIMESTAMP_MICROS` with `isAdjustedToUTC = false`. No independent Parquet reader
(pyarrow, fastparquet, parquet-tools) was available on the machine this was
developed on and none was installed. The only reader-independent check is
structural: the file starts and ends with `PAR1`, and the footer length field
(22,427 bytes) lies inside the file. If you load it somewhere else and a type or
a timestamp comes out differently, that is a bug worth reporting.

## Reading it

Any Parquet reader works:

```bash
duckdb -c "SELECT date_trunc('day', timestamp) AS day, count(*) FROM 'example.com-2026-03.parquet' GROUP BY 1 ORDER BY 1"
```

```python
import pandas as pd
df = pd.read_parquet("example.com-2026-03.parquet")   # needs pyarrow or fastparquet
```

`timestamp` loads as a timezone-naive value; treat it as UTC.
