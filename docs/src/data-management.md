# Data Management

## Storage Layout

Events are stored as date-partitioned, ZSTD-compressed Parquet files:

```
data/events/
├── site_id=example.com/
│   ├── date=2024-01-15/
│   │   ├── 0001.parquet   # first flush for this day
│   │   └── 0002.parquet   # second flush for this day
│   └── date=2024-01-16/
│       └── 0001.parquet
└── site_id=other.org/
    └── date=2024-01-15/
        └── 0001.parquet
```

Each Parquet file contains one batch of flushed events for a specific site and date. Files are numbered sequentially within each partition.

---

## Buffer and Flush Lifecycle

```
Events arrive → in-memory Vec<Event> buffer
                    │
         threshold or timer fires
                    │
                    ▼
           INSERT INTO events table (DuckDB)
                    │
                    ▼
         COPY TO Parquet file (ZSTD)
                    │
                    ▼
         DELETE FROM events table
                    │
                    ▼
         Refresh events_all view
```

After a flush, the hot `events` table is cleared. The `events_all` view unions the (now-empty) hot table with the newly written Parquet files, keeping all data queryable.

**Flush triggers:**
1. Event count reaches `flush_event_count` (default 1000).
2. Periodic timer fires every `flush_interval_secs` (default 60 seconds).
3. Graceful shutdown (bounded by `shutdown_timeout_secs`).

---

## Data Retention

When `retention_days` is set to a non-zero value, a background task runs daily and removes Parquet partition directories older than the configured threshold.

```toml
# Delete partitions older than 90 days
retention_days = 90
```

**What is deleted:** the entire `date=YYYY-MM-DD/` directory and all Parquet files within it.

**What is not deleted:** the `site_id=*/` parent directory (it remains even if all date partitions have been removed).

To keep data indefinitely, set `retention_days = 0` (the default).

### GDPR Right to Erasure

Mallard Metrics stores no PII (IP addresses are discarded after hashing, visitor IDs are pseudonymous and daily-rotating). There is no direct mechanism to delete a specific visitor's data because the visitor ID cannot be reverse-mapped to an individual.

---

## Backup

Parquet files are self-describing and can be read by any Parquet-compatible tool (DuckDB, Apache Spark, pandas, etc.). To back up:

```bash
# Copy data directory to backup location
rsync -a --checksum /data/events/ /backup/mallard-events/

# Or with rclone to S3
rclone sync /data/events s3:my-bucket/mallard-events
```

To restore:

```bash
rsync -a /backup/mallard-events/ /data/events/
```

After restore, restart the Mallard Metrics server. The `events_all` view will automatically pick up the restored Parquet files.

---

## Inspecting Data with DuckDB CLI

You can query Parquet files directly with the DuckDB CLI:

```bash
# Install DuckDB CLI
# https://duckdb.org/docs/installation/

duckdb

-- Query all data for a site
SELECT
    CAST(timestamp AS DATE) AS date,
    COUNT(DISTINCT visitor_id) AS visitors,
    COUNT(*) FILTER (WHERE event_name = 'pageview') AS pageviews
FROM read_parquet('data/events/site_id=example.com/**/*.parquet')
GROUP BY date
ORDER BY date;

-- Top pages
SELECT pathname, COUNT(*) AS views
FROM read_parquet('data/events/site_id=example.com/**/*.parquet')
WHERE event_name = 'pageview'
  AND CAST(timestamp AS DATE) >= '2024-01-01'
GROUP BY pathname
ORDER BY views DESC
LIMIT 20;
```

---

## Schema

The events table schema (also the Parquet file schema):

| Column | Type | Nullable | Description |
|---|---|---|---|
| `site_id` | VARCHAR | No | Site identifier |
| `visitor_id` | VARCHAR | No | HMAC-SHA256 privacy-safe visitor ID |
| `timestamp` | TIMESTAMP | No | UTC event timestamp |
| `event_name` | VARCHAR | No | Event type (e.g. `pageview`, `signup`) |
| `pathname` | VARCHAR | No | URL path |
| `hostname` | VARCHAR | Yes | URL hostname |
| `referrer` | VARCHAR | Yes | Referrer URL |
| `referrer_source` | VARCHAR | Yes | Parsed referrer source name |
| `utm_source` | VARCHAR | Yes | UTM source parameter |
| `utm_medium` | VARCHAR | Yes | UTM medium parameter |
| `utm_campaign` | VARCHAR | Yes | UTM campaign parameter |
| `utm_content` | VARCHAR | Yes | UTM content parameter |
| `utm_term` | VARCHAR | Yes | UTM term parameter |
| `browser` | VARCHAR | Yes | Browser name |
| `browser_version` | VARCHAR | Yes | Browser version string |
| `os` | VARCHAR | Yes | Operating system name |
| `os_version` | VARCHAR | Yes | OS version string |
| `device_type` | VARCHAR | Yes | `desktop`, `mobile`, or `tablet` |
| `screen_size` | VARCHAR | Yes | Screen dimensions (e.g. `1920x1080`) |
| `country_code` | VARCHAR(2) | Yes | ISO 3166-1 alpha-2 country code |
| `region` | VARCHAR | Yes | Region/state name |
| `city` | VARCHAR | Yes | City name |
| `props` | VARCHAR | Yes | Custom properties (JSON string) |
| `revenue_amount` | DECIMAL(12,2) | Yes | Revenue amount |
| `revenue_currency` | VARCHAR(3) | Yes | ISO 4217 currency code |
