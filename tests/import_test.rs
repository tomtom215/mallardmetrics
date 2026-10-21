//! Imported rows, seen through the real router.
//!
//! The differential test imports a GA4 export into a file-backed instance and
//! pushes the same events through the live ingest buffer into another, then
//! requires every report to be identical. The remaining tests pin the
//! documented behaviour of imported rows in the visitor, session and retention
//! metrics (docs/src/importing-data.md#how-imported-rows-behave).

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use mallard_metrics::import::{self, Format, Options, Parsed, Policy, ga4};
use mallard_metrics::ingest::buffer::Event;
use mallard_metrics::ingest::handler::AppState;
use mallard_metrics::server::build_router;
use mallard_metrics::storage::ReaderPool;
use mallard_metrics::storage::parquet::ParquetStorage;
use mallard_metrics::test_support::{require_behavioral, state_builder};
use parking_lot::Mutex;
use std::sync::Arc;
use tower::ServiceExt;

const T0: i64 = 1_773_489_600_000_000; // 2026-03-14 12:00:00 UTC
const NOW: i64 = 1_800_000_000;

fn options() -> Options {
    Options {
        format: Format::Ga4BigQuery,
        site_id: "example.com".to_string(),
        key: b"integration-test-key".to_vec(),
        policy: Policy::default(),
        include_all_events: false,
        skip_invalid: false,
        now_secs: NOW,
    }
}

fn row(user: &str, session: i64, offset_secs: i64, name: &str, path: &str, extra: &str) -> String {
    format!(
        r#"{{"event_timestamp":"{ts}","event_name":"{name}","event_params":[{{"key":"page_location","value":{{"string_value":"https://example.com{path}"}}}},{{"key":"page_referrer","value":{{"string_value":"https://news.ycombinator.com/item?id=1"}}}},{{"key":"ga_session_id","value":{{"int_value":"{session}"}}}},{{"key":"currency","value":{{"string_value":"USD"}}}}],"user_pseudo_id":"{user}","platform":"WEB","device":{{"category":"mobile","operating_system":"Android","operating_system_version":"14","web_info":{{"browser":"Chrome","browser_version":"122"}}}},"geo":{{"region":"Ontario","city":"Toronto"}}{extra}}}"#,
        ts = T0 + offset_secs * 1_000_000
    )
}

/// Three GA4 sessions: a bounce, a three-page visit with a purchase, and a
/// one-page visit by the first user a day later.
fn export() -> Vec<String> {
    vec![
        row("u1", 1, 0, "page_view", "/", ""),
        row("u2", 2, 60, "page_view", "/pricing", ""),
        row("u2", 2, 120, "page_view", "/docs", ""),
        row(
            "u2",
            2,
            180,
            "purchase",
            "/thanks",
            r#","ecommerce":{"purchase_revenue":42.5}"#,
        ),
        row("u1", 3, 86_400, "page_view", "/", ""),
    ]
}

fn events(lines: &[String]) -> Vec<Event> {
    lines
        .iter()
        .map(|l| {
            match ga4::parse_line(
                l,
                "example.com",
                &Policy::default(),
                &options().key,
                false,
                NOW,
            )
            .unwrap()
            {
                Parsed::Event(e) => *e,
                Parsed::Skip(why) => panic!("skipped: {why}"),
            }
        })
        .collect()
}

/// An instance whose data came from the importer.
fn imported_instance(lines: &[String]) -> (Arc<AppState>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let events_dir = dir.path().join("events");
    std::fs::create_dir_all(&events_dir).unwrap();
    let conn = duckdb::Connection::open(dir.path().join("mallard.duckdb")).unwrap();
    let storage = ParquetStorage::new(&events_dir, 0);
    import::prepare(&conn, &storage).unwrap();
    let report = import::import(
        &conn,
        &storage,
        std::io::Cursor::new(lines.join("\n").into_bytes()),
        &options(),
    )
    .unwrap();
    assert_eq!(report.inserted, lines.len() as u64);
    // The server would load the extension on its connections; do the same.
    let behavioral = mallard_metrics::storage::schema::load_behavioral_extension(&conn).is_ok();
    let writer = Arc::new(Mutex::new(conn));
    let readers = ReaderPool::shared(&writer);
    let state = state_builder()
        .behavioral_extension_loaded(behavioral)
        .build_over(writer, readers, &events_dir);
    (state, dir)
}

/// An instance that received the same events through the live buffer.
fn live_instance(lines: &[String]) -> (Arc<AppState>, tempfile::TempDir) {
    let (state, dir) = state_builder().build();
    for event in events(lines) {
        state.buffer.push(event).unwrap();
    }
    state.buffer.flush().unwrap();
    (state, dir)
}

async fn get_json(state: &Arc<AppState>, uri: &str) -> (u16, serde_json::Value) {
    let response = build_router(Arc::clone(state))
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or_default())
}

const RANGE: &str = "site_id=example.com&start_date=2026-03-14&end_date=2026-03-15";

#[tokio::test]
async fn imported_rows_report_identically_to_live_ones() {
    let (imported, _a) = imported_instance(&export());
    let (live, _b) = live_instance(&export());

    let mut compared = 0;
    for endpoint in [
        "stats/main",
        "stats/timeseries",
        "stats/sessions",
        "stats/revenue",
        "stats/goals",
        "stats/breakdown/pages",
        "stats/breakdown/sources",
        "stats/breakdown/browsers",
        "stats/breakdown/os",
        "stats/breakdown/devices",
        "stats/breakdown/regions",
        "stats/breakdown/cities",
        "stats/breakdown/entry-pages",
        "stats/breakdown/exit-pages",
    ] {
        let uri = format!("/api/{endpoint}?{RANGE}");
        let (status_a, a) = get_json(&imported, &uri).await;
        let (status_b, b) = get_json(&live, &uri).await;
        assert_eq!(status_a, status_b, "{endpoint}");
        assert_eq!(a, b, "{endpoint} differs between imported and live rows");
        compared += 1;
    }
    assert_eq!(compared, 14);

    // And the numbers are the right ones, not merely equal.
    let (_, main) = get_json(&imported, &format!("/api/stats/main?{RANGE}")).await;
    assert_eq!(main["total_pageviews"], 4);
    assert_eq!(main["total_events"], 5);
    assert_eq!(main["unique_visitors"], 3, "one visitor per GA4 session");
}

#[tokio::test]
async fn a_raw_export_of_imported_rows_has_no_visitor_ids() {
    let (state, _dir) = imported_instance(&export());
    let response = build_router(state)
        .oneshot(
            Request::builder()
                .uri(format!("/api/stats/export?{RANGE}&kind=raw&format=csv"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("/pricing"));
    assert!(
        !text.contains("imp:") && !text.contains("visitor_id"),
        "{text}"
    );
}

#[tokio::test]
async fn sessions_follow_ga4_sessions_and_bounce_means_one_pageview() {
    let (state, _dir) = imported_instance(&export());
    if !require_behavioral(&state, "imported session metrics") {
        return;
    }
    let (status, sessions) = get_json(&state, &format!("/api/stats/sessions?{RANGE}")).await;
    assert_eq!(status, 200, "{sessions}");
    assert_eq!(sessions["total_sessions"], 3, "{sessions}");
    // Two of the three sessions are single-pageview visits.
    let bounce = sessions["bounce_rate"].as_f64().unwrap();
    assert!((bounce - 2.0 / 3.0).abs() < 1e-9, "{sessions}");
}

#[tokio::test]
async fn one_ga4_user_is_not_a_returning_visitor() {
    // u1 has a session on each of the two days; the importer deliberately gives
    // them unrelated ids, so retention sees two new visitors, not one returning.
    let (state, _dir) = imported_instance(&export());
    let (_, main) = get_json(&state, &format!("/api/stats/main?{RANGE}")).await;
    assert_eq!(main["unique_visitors"], 3);
    let (_, series) = get_json(&state, &format!("/api/stats/timeseries?{RANGE}")).await;
    let visitors: u64 = series
        .as_array()
        .map(|a| a.iter().filter_map(|b| b["visitors"].as_u64()).sum())
        .unwrap_or_default();
    assert_eq!(visitors, 3, "{series}");
}

#[tokio::test]
async fn imported_ids_cannot_collide_with_live_ones() {
    // Live ids are 64 hex characters; imported ones start with `imp:` and are
    // shorter, so a live visitor can never be merged into an imported session.
    let (state, _dir) = imported_instance(&export());
    let conn = state.buffer.conn().lock();
    let ids: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events_all WHERE visitor_id NOT LIKE 'imp:%' OR LENGTH(visitor_id) = 64",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ids, 0);
}
