use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use duckdb::Connection;
use mallard_metrics::ingest::buffer::{Event, EventBuffer};
use mallard_metrics::storage::parquet::ParquetStorage;
use mallard_metrics::storage::schema;
use parking_lot::Mutex;
use std::sync::Arc;

fn make_event(i: usize) -> Event {
    Event {
        site_id: "bench.example.com".to_string(),
        visitor_id: format!("visitor-{}", i % 1000),
        timestamp: chrono::NaiveDate::from_ymd_opt(2024, 1, 15)
            .unwrap()
            .and_hms_opt(
                10,
                u32::try_from(i / 60).unwrap_or(0) % 24,
                u32::try_from(i % 60).unwrap_or(0),
            )
            .unwrap(),
        event_name: "pageview".to_string(),
        pathname: format!("/page-{}", i % 100),
        hostname: Some("bench.example.com".to_string()),
        referrer: None,
        referrer_source: None,
        utm_source: None,
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
        revenue_amount: None,
        revenue_currency: None,
    }
}

fn bench_buffer_push(c: &mut Criterion) {
    let mut group = c.benchmark_group("ingest_throughput");

    for size in [100, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let conn = Connection::open_in_memory().unwrap();
                schema::init_schema(&conn).unwrap();
                let dir = tempfile::tempdir().unwrap();
                let storage = ParquetStorage::new(dir.path());
                let conn = Arc::new(Mutex::new(conn));
                // Set threshold above test size to avoid auto-flush during push
                let buffer = EventBuffer::new(size + 1, conn, storage);

                for i in 0..size {
                    buffer.push(make_event(i)).unwrap();
                }
            });
        });
    }

    group.finish();
}

fn bench_flush(c: &mut Criterion) {
    let mut group = c.benchmark_group("parquet_flush");

    for size in [1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(size), &size, |b, &size| {
            b.iter(|| {
                let conn = Connection::open_in_memory().unwrap();
                schema::init_schema(&conn).unwrap();
                let dir = tempfile::tempdir().unwrap();
                let storage = ParquetStorage::new(dir.path());
                let conn = Arc::new(Mutex::new(conn));
                let buffer = EventBuffer::new(size + 1, Arc::clone(&conn), storage);

                for i in 0..size {
                    buffer.push(make_event(i)).unwrap();
                }
                buffer.flush().unwrap();
            });
        });
    }

    group.finish();
}

criterion_group!(benches, bench_buffer_push, bench_flush);
criterion_main!(benches);
