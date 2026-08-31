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

criterion_group!(
    benches,
    bench_buffer_push,
    bench_flush,
    bench_queries,
    bench_user_agent,
    bench_visitor_id
);
criterion_main!(benches);
