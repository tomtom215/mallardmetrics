use criterion::{BatchSize, BenchmarkId, Criterion, criterion_group, criterion_main};
use duckdb::Connection;
use mallard_metrics::ingest::buffer::{Event, EventBuffer};
use mallard_metrics::query::QueryScope;
use mallard_metrics::query::{breakdowns, metrics, timeseries};
use mallard_metrics::storage::TierLock;
use mallard_metrics::storage::parquet::ParquetStorage;
use mallard_metrics::storage::schema;
use parking_lot::Mutex;
use std::sync::Arc;

/// Site used by every benchmark.
const SITE: &str = "bench.example.com";

/// Build a synthetic event with realistic cardinality: 1000 visitors across
/// 100 pages, spread over a day.
fn make_event(i: usize) -> Event {
    Event {
        site_id: SITE.to_string(),
        visitor_id: format!("visitor-{}", i % 1000),
        timestamp: chrono::NaiveDate::from_ymd_opt(2024, 1, 15)
            .unwrap()
            .and_hms_micro_opt(
                u32::try_from(i / 3600).unwrap_or(0) % 24,
                u32::try_from(i / 60).unwrap_or(0) % 60,
                u32::try_from(i).unwrap_or(0) % 60,
                u32::try_from(i).unwrap_or(0) % 1_000_000,
            )
            .unwrap(),
        event_name: if i.is_multiple_of(20) {
            "signup"
        } else {
            "pageview"
        }
        .to_string(),
        pathname: format!("/page-{}", i % 100),
        hostname: Some(SITE.to_string()),
        referrer: None,
        referrer_source: Some(
            if i.is_multiple_of(3) {
                "Google"
            } else {
                "Direct"
            }
            .to_string(),
        ),
        utm_source: Some(format!("campaign-{}", i % 5)),
        utm_medium: None,
        utm_campaign: None,
        utm_content: None,
        utm_term: None,
        browser: Some("Chrome".to_string()),
        browser_version: Some("120.0".to_string()),
        os: Some("Linux".to_string()),
        os_version: Some("6.1".to_string()),
        device_type: Some("desktop".to_string()),
        screen_size: Some("1920".to_string()),
        country_code: Some("US".to_string()),
        region: None,
        city: None,
        props: None,
        revenue_amount: i.is_multiple_of(50).then_some(19.99),
        revenue_currency: i.is_multiple_of(50).then(|| "USD".to_string()),
    }
}

/// A warm in-memory database with an `events_all` view over `dir`.
fn warm_db(dir: &std::path::Path) -> Arc<Mutex<Connection>> {
    let conn = Connection::open_in_memory().unwrap();
    schema::init_schema(&conn).unwrap();
    schema::setup_query_view(&conn, dir).unwrap();
    Arc::new(Mutex::new(conn))
}

/// Steady-state buffer push on a warm connection.
///
/// Setup runs outside `b.iter()` so only the push cost is measured. When it was
/// inside, DuckDB's ~500 ms cold start dominated every result — the tell was
/// that 100 and 1000 events timed almost identically. See PERF.md.
fn bench_buffer_push(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingest_throughput");

    for size in [100, 1_000, 10_000] {
        let dir = tempfile::tempdir().unwrap();
        let conn = warm_db(dir.path());
        let storage = ParquetStorage::new(dir.path(), 0);
        // A threshold above `size` keeps auto-flush from firing mid-measurement.
        let buffer = EventBuffer::new(size + 1, 0, conn, storage, TierLock::new());

        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                for i in 0..size {
                    buffer.push(make_event(i)).unwrap();
                }
                // Reset outside the measured push loop.
                buffer.flush().unwrap();
            });
        });
    }

    group.finish();
}

/// Steady-state Parquet flush.
///
/// `iter_batched` keeps connection setup and event population out of the
/// measurement; only `flush()` is timed.
fn bench_flush(c: &mut Criterion) {
    let mut group = c.benchmark_group("parquet_flush");

    for size in [1_000, 10_000] {
        let dir = tempfile::TempDir::new().unwrap();
        let dir_path = dir.path().to_path_buf();

        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter_batched(
                || {
                    let conn = warm_db(&dir_path);
                    let storage = ParquetStorage::new(&dir_path, 0);
                    let buffer = EventBuffer::new(size + 1, 0, conn, storage, TierLock::new());
                    for i in 0..size {
                        buffer.push(make_event(i)).unwrap();
                    }
                    buffer
                },
                |buffer| {
                    buffer.flush().unwrap();
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// Read-path benchmarks over a 10k-event dataset.
fn bench_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("query");

    let dir = tempfile::tempdir().unwrap();
    let conn = warm_db(dir.path());
    let storage = ParquetStorage::new(dir.path(), 0);
    let buffer = EventBuffer::new(20_000, 0, Arc::clone(&conn), storage, TierLock::new());

    for i in 0..10_000 {
        buffer.push(make_event(i)).unwrap();
    }

    let scope = QueryScope::new(SITE, "2024-01-01", "2024-02-01", "30 minutes");

    // Core metrics used to issue four independent scans of events_all; this
    // measures the combined implementation.
    group.bench_function("core_metrics_10k", |b| {
        b.iter(|| {
            let guard = conn.lock();
            metrics::query_core_metrics(&guard, &scope).unwrap();
        });
    });

    group.bench_function("timeseries_10k", |b| {
        b.iter(|| {
            let guard = conn.lock();
            timeseries::query_timeseries(&guard, &scope, timeseries::Granularity::Day).unwrap();
        });
    });

    group.bench_function("breakdown_pages_10k", |b| {
        b.iter(|| {
            let guard = conn.lock();
            breakdowns::query_breakdown(&guard, &scope, breakdowns::Dimension::Page, 10).unwrap();
        });
    });

    group.bench_function("breakdown_utm_source_10k", |b| {
        b.iter(|| {
            let guard = conn.lock();
            breakdowns::query_breakdown(&guard, &scope, breakdowns::Dimension::UtmSource, 10)
                .unwrap();
        });
    });

    group.bench_function("goals_10k", |b| {
        b.iter(|| {
            let guard = conn.lock();
            mallard_metrics::query::events::query_goals(&guard, &scope).unwrap();
        });
    });

    group.bench_function("revenue_10k", |b| {
        b.iter(|| {
            let guard = conn.lock();
            mallard_metrics::query::revenue::query_revenue(&guard, &scope).unwrap();
        });
    });

    group.finish();
}

/// User-agent parsing, which runs on every ingested event.
fn bench_user_agent(c: &mut Criterion) {
    use mallard_metrics::ingest::useragent::parse_user_agent;

    const AGENTS: &[&str] = &[
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/120.0.0.0 Safari/537.36",
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) \
         Version/17.2 Safari/605.1.15",
        "Mozilla/5.0 (compatible; Googlebot/2.1; +http://www.google.com/bot.html)",
        "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Chrome/120.0.0.0 Mobile",
    ];

    c.bench_function("parse_user_agent", |b| {
        b.iter(|| {
            for ua in AGENTS {
                std::hint::black_box(parse_user_agent(ua));
            }
        });
    });
}

/// Visitor-ID derivation, which also runs on every ingested event.
fn bench_visitor_id(c: &mut Criterion) {
    use mallard_metrics::ingest::visitor_id::{generate_visitor_id, rotating_salt};

    let salt = rotating_salt(
        "bench-secret",
        chrono::NaiveDate::from_ymd_opt(2024, 1, 15).unwrap(),
        1,
    );

    c.bench_function("generate_visitor_id", |b| {
        b.iter(|| {
            std::hint::black_box(generate_visitor_id(
                SITE,
                "203.0.113.5",
                "Mozilla/5.0 (Windows NT 10.0) Chrome/120.0",
                &salt,
            ));
        });
    });
}

/// Number of sites and days in the synthetic cold tier. The headline case in
/// the audit (F-06) is 100 sites x 90 days = 9,000 one-file partitions, which
/// is what compaction leaves behind. Overridable so a laptop can try a smaller
/// shape: `MALLARD_BENCH_SITES=20 MALLARD_BENCH_DAYS=30`.
///
/// `cargo test --all-targets` also runs this binary, without `--bench`. Writing
/// 18,000 files there would add minutes to every test run, so that mode uses a
/// token shape that still exercises the code.
fn scale_shape() -> (usize, usize) {
    let measuring = std::env::args().any(|a| a == "--bench");
    let (default_sites, default_days) = if measuring { (100, 90) } else { (5, 4) };
    let read = |key: &str, default: usize| {
        std::env::var(key)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    (
        read("MALLARD_BENCH_SITES", default_sites),
        read("MALLARD_BENCH_DAYS", default_days),
    )
}

/// Write one compacted-looking partition file per (site, day).
///
/// `with_site_id` selects the on-disk layout: the legacy one carries `site_id`
/// as a column inside the file, the current one leaves it to the directory name.
fn write_scale_dataset(root: &std::path::Path, with_site_id: bool) {
    let (sites, days) = scale_shape();
    let conn = Connection::open_in_memory().unwrap();
    let projection = if with_site_id {
        "*"
    } else {
        "* EXCLUDE (site_id)"
    };
    let start = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
    for s in 0..sites {
        let site = format!("site{s:03}.example.com");
        for d in 0..days {
            let date = start + chrono::Duration::days(i64::try_from(d).unwrap());
            let dir = root
                .join(format!("site_id={site}"))
                .join(format!("date={date}"));
            std::fs::create_dir_all(&dir).unwrap();
            let file = dir.join("0001.parquet");
            conn.execute_batch(&format!(
                "COPY (SELECT {projection} FROM (\
                   SELECT '{site}' AS site_id, 'v' || (i % 17) AS visitor_id, \
                          TIMESTAMP '{date} 00:00:00' + INTERVAL (i * 17) MINUTE AS timestamp, \
                          'pageview' AS event_name, '/p' || (i % 5) AS pathname, \
                          CAST(NULL AS VARCHAR) AS hostname, CAST(NULL AS VARCHAR) AS referrer, \
                          CAST(NULL AS VARCHAR) AS referrer_source, CAST(NULL AS VARCHAR) AS utm_source, \
                          CAST(NULL AS VARCHAR) AS utm_medium, CAST(NULL AS VARCHAR) AS utm_campaign, \
                          CAST(NULL AS VARCHAR) AS utm_content, CAST(NULL AS VARCHAR) AS utm_term, \
                          'Chrome' AS browser, CAST(NULL AS VARCHAR) AS browser_version, \
                          'Linux' AS os, CAST(NULL AS VARCHAR) AS os_version, \
                          'desktop' AS device_type, CAST(NULL AS VARCHAR) AS screen_size, \
                          'US' AS country_code, CAST(NULL AS VARCHAR) AS region, \
                          CAST(NULL AS VARCHAR) AS city, CAST(NULL AS VARCHAR) AS props, \
                          CAST(NULL AS DECIMAL(12,2)) AS revenue_amount, \
                          CAST(NULL AS VARCHAR) AS revenue_currency \
                   FROM range(60) t(i))) \
                 TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                file.to_string_lossy()
            ))
            .unwrap();
        }
    }
}

/// Per-request cost of the cold tier as it grows (audit F-06).
///
/// Every dashboard request is `WHERE site_id = ? AND timestamp BETWEEN ..` over
/// `events_all`. This builds the same logical dataset in the legacy file layout
/// and in the layout without a `site_id` column, and times that query through
/// each view definition, so the claim about what pruning buys is a measurement.
/// Setup is outside `b.iter()` (L21).
fn bench_query_scale(c: &mut Criterion) {
    // A site in the middle of the dataset and a week from the middle of its
    // history (2026-08-15..22 at the default 90 days).
    let (sites, days) = scale_shape();
    let target_site = format!("site{:03}.example.com", sites / 2);
    let first_day = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()
        + chrono::Duration::days(i64::try_from(days.saturating_sub(7) / 2).unwrap());
    let last_day = first_day + chrono::Duration::days(6);
    let range = (
        first_day.format("%Y-%m-%d").to_string(),
        (last_day + chrono::Duration::days(1))
            .format("%Y-%m-%d")
            .to_string(),
    );
    let legacy = tempfile::tempdir().unwrap();
    let lean = tempfile::tempdir().unwrap();
    write_scale_dataset(legacy.path(), true);
    write_scale_dataset(lean.path(), false);

    let columns = schema::EVENT_COLUMNS.join(", ");
    let hive = "hive_partitioning=true, hive_types={'site_id': VARCHAR, 'date': DATE}";
    // (label, files, read_parquet options, per-connection setup)
    let cache = "SET GLOBAL parquet_metadata_cache = true";
    let variants: Vec<(&str, &std::path::Path, String, &str)> = vec![
        (
            "current_union_by_name",
            legacy.path(),
            "union_by_name=true, hive_partitioning=false".to_string(),
            "",
        ),
        (
            "current_plus_metadata_cache",
            legacy.path(),
            "union_by_name=true, hive_partitioning=false".to_string(),
            cache,
        ),
        (
            "legacy_hive_union_by_name",
            legacy.path(),
            format!("union_by_name=true, {hive}"),
            "",
        ),
        (
            "legacy_hive_plus_metadata_cache",
            legacy.path(),
            format!("union_by_name=true, {hive}"),
            cache,
        ),
        (
            "lean_hive_union_by_name",
            lean.path(),
            format!("union_by_name=true, {hive}"),
            "",
        ),
        (
            "legacy_no_union_by_name",
            legacy.path(),
            "hive_partitioning=false".to_string(),
            "",
        ),
        ("lean_hive", lean.path(), hive.to_string(), ""),
        (
            "lean_hive_plus_metadata_cache",
            lean.path(),
            hive.to_string(),
            cache,
        ),
    ];

    let mut group = c.benchmark_group("query_scale");
    group.sample_size(10);
    for (label, root, options, setup) in variants {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_schema(&conn).unwrap();
        if !setup.is_empty() {
            conn.execute_batch(setup).unwrap();
        }
        conn.execute_batch(&format!(
            "CREATE VIEW events_all AS SELECT {columns} FROM events UNION ALL \
             SELECT {columns} FROM read_parquet('{}/site_id=*/date=*/*.parquet', {options})",
            root.to_string_lossy()
        ))
        .unwrap();
        group.bench_function(label, |b| {
            b.iter(|| {
                let n: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM events_all WHERE site_id = ? \
                         AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)",
                        duckdb::params![target_site, range.0, range.1],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(n > 0);
            });
        });
    }
    // The scoped read the query layer can use: the same question answered from
    // the files of one site and week only.
    {
        let conn = Connection::open_in_memory().unwrap();
        schema::init_schema(&conn).unwrap();
        let storage = ParquetStorage::new(legacy.path(), 0);
        group.bench_function("scoped_file_list", |b| {
            b.iter(|| {
                // The file list is rebuilt per request, as a handler would.
                let source = schema::scoped_events_sql(&storage.files_for_range(
                    &target_site,
                    first_day,
                    last_day,
                ));
                let sql = format!(
                    "SELECT COUNT(*) FROM {source} WHERE site_id = ? \
                     AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)"
                );
                let n: i64 = conn
                    .query_row(
                        &sql,
                        duckdb::params![target_site, range.0, range.1],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert!(n > 0);
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_buffer_push,
    bench_flush,
    bench_queries,
    bench_query_scale,
    bench_user_agent,
    bench_visitor_id
);
criterion_main!(benches);
