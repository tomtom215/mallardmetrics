# Architecture

## Overview

Mallard Metrics is a single Rust binary that handles the full analytics lifecycle: event ingestion, storage, querying, authentication, and dashboard serving. There are no external services, no message queues, and no separate database process.

```
Browser / Client
      │
      ▼  POST /api/event
 ┌────────────────────────────────────────────────────┐
 │                   Axum HTTP Server                 │
 │                                                    │
 │  Ingestion Pipeline                                │
 │  ┌─────────┐  ┌────────────────────┐  ┌────────┐  │
 │  │  Auth / │  │  UA Parse /        │  │        │  │
 │  │  Origin │→ │  GeoIP /           │→ │ Buffer │  │
 │  │  Check  │  │  Visitor ID Hash   │  │        │  │
 │  └─────────┘  └────────────────────┘  └───┬────┘  │
 │                                           │        │
 │                        flush              │        │
 │                   ┌──────────────────────┘        │
 │                   ▼                               │
 │  ┌────────────────────────┐                       │
 │  │   DuckDB (in-memory)   │←──── read_parquet()   │
 │  │   events table (hot)   │       (cold tier)     │
 │  └───────────┬────────────┘                       │
 │              │  COPY TO (flush)                   │
 │              ▼                                    │
 │  ┌────────────────────────┐                       │
 │  │  Parquet files (disk)  │                       │
 │  │  date-partitioned      │                       │
 │  │  ZSTD-compressed       │                       │
 │  └────────────────────────┘                       │
 │                                                    │
 │  Query Layer (GET /api/stats/*)                    │
 │  ┌────────────────────────────────────────────┐   │
 │  │  events_all VIEW = events ∪ read_parquet() │   │
 │  └────────────────────────────────────────────┘   │
 └────────────────────────────────────────────────────┘
```

---

## Two-Tier Storage Model

### Hot Tier — DuckDB in-memory events table

Events that have been received but not yet flushed are stored in the in-memory DuckDB `events` table. These events are immediately queryable.

- Flushed when: buffer count threshold is reached (`flush_event_count`), or the periodic interval fires (`flush_interval_secs`), or on graceful shutdown.
- After flushing: events are written to Parquet files, then deleted from the `events` table.

### Cold Tier — Parquet files on disk

Persisted events are stored as date-partitioned, ZSTD-compressed Parquet files:

```
data/events/
├── site_id=example.com/
│   ├── date=2024-01-15/
│   │   ├── 0001.parquet
│   │   └── 0002.parquet
│   └── date=2024-01-16/
│       └── 0001.parquet
└── site_id=other-site.org/
    └── date=2024-01-15/
        └── 0001.parquet
```

Parquet files are the durability layer. On server restart, DuckDB starts with an empty in-memory table and the cold tier is brought back into scope via the `events_all` view.

### The `events_all` View

At startup and after each flush, Mallard Metrics creates or refreshes a DuckDB view:

```sql
CREATE OR REPLACE VIEW events_all AS
    SELECT * FROM events              -- hot: current session's unflushed events
    UNION ALL
    SELECT * FROM read_parquet(
        'data/events/site_id=*/date=*/*.parquet',
        union_by_name=true
    )                                 -- cold: all persisted Parquet files
```

All analytics queries target `events_all`, not `events` directly. This ensures:
- Queries see recently arrived events immediately.
- Queries see historical data across restarts without data loss.

---

## Event Ingestion Pipeline

1. **Request received** — `POST /api/event` with JSON body.
2. **Origin validation** — If `site_ids` is configured, the `Origin` header is checked against the exact allowlist.
3. **Rate limiting** — Token-bucket limiter per `site_id`. Excess requests receive `429`.
4. **Bot filtering** — User-Agent checked against bot pattern list. Matching events are discarded silently.
5. **User-Agent parsing** — Browser, OS, and device type extracted from the `User-Agent` header.
6. **GeoIP lookup** — Country, region, and city resolved from client IP (if a MaxMind database is configured).
7. **Visitor ID hashing** — `HMAC-SHA256(IP + UA + daily-UTC-date, MALLARD_SECRET)` produces a privacy-safe, daily-rotating visitor identifier. The IP is then discarded.
8. **URL parsing** — Pathname, hostname, UTM parameters extracted from the `u` field.
9. **Buffer push** — The event is added to the in-memory `Vec<Event>` buffer.
10. **Threshold check** — If the buffer length reaches `flush_event_count`, a synchronous flush is triggered.

---

## Behavioral Extension

Advanced analytics (funnels, sessions, retention, sequences, flow) rely on the DuckDB `behavioral` extension. This extension provides window functions:

| Function | Used for |
|---|---|
| `sessionize()` | Session identification and duration |
| `window_funnel()` | Conversion funnel analysis |
| `retention()` | Weekly cohort retention grids |
| `sequence_match()` | Behavioral pattern detection |
| `sequence_next_node()` | Next-page flow analysis |

The extension is loaded at startup with:

```sql
INSTALL behavioral FROM community;
LOAD behavioral;
```

If the extension is not available (network failure, air-gapped environment), all extension-dependent endpoints return graceful defaults (zeroes or empty arrays). Core metrics (visitors, pageviews, breakdowns, timeseries) work without the extension.

---

## Authentication Architecture

- **Dashboard password** — Hashed with Argon2id (PHC default parameters). The hash is stored in memory only; it is set from `MALLARD_ADMIN_PASSWORD` at startup.
- **Sessions** — 256-bit cryptographically random tokens stored in an in-memory `HashMap` with TTL. Delivered as `HttpOnly; SameSite=Strict` cookies.
- **API keys** — Generated with 128 bits of randomness, prefixed `mm_`, SHA-256 hashed before storage. Compared in constant time to prevent timing attacks.

---

## Module Map

| Module | Purpose |
|---|---|
| `config.rs` | TOML + environment variable configuration |
| `server.rs` | Axum router with CORS configuration |
| `ingest/handler.rs` | `POST /api/event` ingestion handler |
| `ingest/buffer.rs` | In-memory event buffer with periodic flush |
| `ingest/visitor_id.rs` | HMAC-SHA256 privacy-safe visitor ID |
| `ingest/useragent.rs` | User-Agent parsing |
| `ingest/geoip.rs` | MaxMind GeoIP reader with graceful fallback |
| `ingest/ratelimit.rs` | Per-site token-bucket rate limiter |
| `storage/schema.rs` | DuckDB table definitions and `events_all` view |
| `storage/parquet.rs` | Parquet write/read/partitioning |
| `storage/migrations.rs` | Schema versioning |
| `query/metrics.rs` | Core metric calculations |
| `query/breakdowns.rs` | Dimension breakdown queries |
| `query/timeseries.rs` | Time-bucketed aggregations |
| `query/sessions.rs` | `sessionize`-based session queries |
| `query/funnel.rs` | `window_funnel` query builder |
| `query/retention.rs` | Retention cohort query execution |
| `query/sequences.rs` | `sequence_match` query execution |
| `query/flow.rs` | `sequence_next_node` flow analysis |
| `query/cache.rs` | TTL-based query result cache |
| `api/stats.rs` | All analytics API handlers |
| `api/errors.rs` | API error types |
| `api/auth.rs` | Origin validation, session auth, API key management |
| `dashboard/` | Embedded SPA (Preact + HTM) |
