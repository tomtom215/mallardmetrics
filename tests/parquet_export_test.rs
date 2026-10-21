//! `GET /api/admin/export/parquet` through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mallard_metrics::api::auth::{ApiKeyScope, ApiKeyStore};
use mallard_metrics::ingest::buffer::Event;
use mallard_metrics::ingest::handler::AppState;
use mallard_metrics::server::build_router;
use mallard_metrics::test_support::state_builder;
use std::sync::Arc;
use tower::ServiceExt;

const ADMIN: &str = "mm_admin_key_for_the_parquet_export_tests";
const READONLY: &str = "mm_readonly_key_for_the_parquet_export_tests";

fn state() -> (Arc<AppState>, tempfile::TempDir) {
    let keys = ApiKeyStore::default();
    keys.add_key("admin", ADMIN, ApiKeyScope::Admin);
    keys.add_key("read", READONLY, ApiKeyScope::ReadOnly);
    state_builder()
        .admin_password("correct-horse-battery")
        .api_keys(keys)
        .build()
}

fn event(site: &str, ts: &str, path: &str) -> Event {
    Event {
        site_id: site.to_string(),
        visitor_id: "f".repeat(64),
        timestamp: chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S%.f").unwrap(),
        event_name: "pageview".to_string(),
        pathname: path.to_string(),
        hostname: Some(site.to_string()),
        referrer: None,
        referrer_source: None,
        utm_source: None,
        utm_medium: None,
        utm_campaign: None,
        utm_content: None,
        utm_term: None,
        browser: Some("Firefox".to_string()),
        browser_version: None,
        os: None,
        os_version: None,
        device_type: Some("desktop".to_string()),
        screen_size: None,
        country_code: Some("NL".to_string()),
        region: None,
        city: None,
        props: Some(r#"{"a":1}"#.to_string()),
        revenue_amount: Some(12.5),
        revenue_currency: Some("EUR".to_string()),
    }
}

async fn get(
    state: &Arc<AppState>,
    uri: &str,
    key: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder().uri(uri);
    if let Some(key) = key {
        request = request.header("x-api-key", key);
    }
    let response = build_router(Arc::clone(state))
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status(), response.headers().clone());
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

const URI: &str =
    "/api/admin/export/parquet?site_id=a.com&start_date=2026-03-14&end_date=2026-03-15";

/// Cold file for one day and a hot row for another, plus a different site.
fn seed(state: &Arc<AppState>) {
    state
        .buffer
        .push(event("a.com", "2026-03-14 10:00:00.250000", "/cold"))
        .unwrap();
    state
        .buffer
        .push(event("a.com", "2026-03-14 11:00:00", "/cold2"))
        .unwrap();
    state
        .buffer
        .push(event("b.com", "2026-03-14 11:00:00", "/other-site"))
        .unwrap();
    state.buffer.flush().unwrap();
    // Left in the hot table: pushed after the flush, inserted by a second one
    // that cannot move it (no partition cap here, so write it directly).
    let conn = state.buffer.conn().lock();
    conn.execute(
        "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname) \
         VALUES ('a.com', 'x', TIMESTAMP '2026-03-15 09:00:00', 'pageview', '/hot')",
        [],
    )
    .unwrap();
}

fn read_back(bytes: &[u8], sql: &str) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.parquet");
    std::fs::write(&path, bytes).unwrap();
    let conn = duckdb::Connection::open_in_memory().unwrap();
    let sql = sql.replace("FILE", &format!("read_parquet('{}')", path.display()));
    conn.prepare(&sql)
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[tokio::test]
async fn admin_scope_is_required() {
    let (state, _dir) = state();
    assert_eq!(get(&state, URI, None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(
        get(&state, URI, Some(READONLY)).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(get(&state, URI, Some(ADMIN)).await.0, StatusCode::OK);
}

#[tokio::test]
async fn the_file_is_parquet_with_the_headers_a_download_needs() {
    let (state, dir) = state();
    seed(&state);
    let (status, headers, body) = get(&state, URI, Some(ADMIN)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "application/vnd.apache.parquet");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=\"mallard-events-a.com-2026-03-14.parquet\""
    );
    assert_eq!(
        headers["content-length"].to_str().unwrap(),
        body.len().to_string()
    );
    assert_eq!(headers["x-mallard-rows"], "3");
    assert_eq!(headers["cache-control"], "no-store");
    assert_eq!(&body[..4], b"PAR1");
    assert_eq!(&body[body.len() - 4..], b"PAR1");

    // Cold and hot rows of the requested site and days, nothing else, in order.
    let paths = read_back(&body, "SELECT pathname FROM FILE ORDER BY timestamp");
    assert_eq!(paths, ["/cold", "/cold2", "/hot"]);
    let columns = read_back(
        &body,
        "SELECT column_name FROM (DESCRIBE SELECT * FROM FILE)",
    );
    assert!(!columns.iter().any(|c| c == "visitor_id"));
    assert_eq!(columns.last().map(String::as_str), Some("is_imported"));
    let micros = read_back(
        &body,
        "SELECT CAST(timestamp AS VARCHAR) FROM FILE ORDER BY timestamp LIMIT 1",
    );
    assert_eq!(micros, ["2026-03-14 10:00:00.25"]);
    let money = read_back(
        &body,
        "SELECT CAST(revenue_amount AS VARCHAR) || revenue_currency FROM FILE WHERE revenue_amount IS NOT NULL LIMIT 1",
    );
    assert_eq!(money, ["12.50EUR"]);

    // Nothing is left in the exports directory once the body has been read.
    drop(body);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let left: Vec<_> = std::fs::read_dir(dir.path().join("exports"))
        .unwrap()
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn a_range_with_no_events_is_a_valid_empty_file() {
    let (state, _dir) = state();
    let (status, headers, body) = get(
        &state,
        "/api/admin/export/parquet?site_id=nobody.com&start_date=2026-03-14&end_date=2026-03-14",
        Some(ADMIN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["x-mallard-rows"], "0");
    assert_eq!(
        read_back(&body, "SELECT CAST(COUNT(*) AS VARCHAR) FROM FILE"),
        ["0"]
    );
}

#[tokio::test]
async fn bad_requests_are_400_and_write_nothing() {
    let (state, dir) = state();
    for uri in [
        "/api/admin/export/parquet?site_id=..%2Fescape&start_date=2026-03-14&end_date=2026-03-14",
        "/api/admin/export/parquet?site_id=a.com%27%20OR%20%271%27%3D%271&start_date=2026-03-14&end_date=2026-03-14",
        "/api/admin/export/parquet?site_id=a.com&start_date=2026-03-14",
        "/api/admin/export/parquet?site_id=a.com&start_date=2025-01-01&end_date=2026-12-31",
        "/api/admin/export/parquet?site_id=a.com&start_date=nope&end_date=2026-03-14",
        "/api/admin/export/parquet",
    ] {
        assert_eq!(
            get(&state, uri, Some(ADMIN)).await.0,
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
    let leftovers = std::fs::read_dir(dir.path().join("exports")).map_or(0, Iterator::count);
    assert_eq!(leftovers, 0);
}

#[tokio::test]
async fn nothing_is_written_outside_the_data_directory() {
    let (state, dir) = state();
    seed(&state);
    let before: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    let _ = get(&state, URI, Some(ADMIN)).await;
    let after: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    for name in &after {
        assert!(
            before.contains(name) || name == "exports",
            "unexpected new entry {name:?}"
        );
    }
}
