use axum::body::Body;
use axum::http::{Request, StatusCode};
use duckdb::Connection;
use http_body_util::BodyExt;
use mallard_metrics::ingest::buffer::EventBuffer;
use mallard_metrics::ingest::handler::AppState;
use mallard_metrics::server::build_router;
use mallard_metrics::storage::parquet::ParquetStorage;
use mallard_metrics::storage::schema;
use parking_lot::Mutex;
use std::sync::Arc;
use tower::ServiceExt;

fn make_test_state() -> (Arc<AppState>, tempfile::TempDir) {
    let conn = Connection::open_in_memory().unwrap();
    schema::init_schema(&conn).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let storage = ParquetStorage::new(dir.path());
    let conn = Arc::new(Mutex::new(conn));
    let buffer = EventBuffer::new(1000, conn, storage);
    let state = Arc::new(AppState {
        buffer,
        secret: "test-secret-integration".to_string(),
        allowed_sites: Vec::new(),
    });
    (state, dir)
}

#[tokio::test]
async fn test_full_ingest_pipeline() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    // Send an event
    let payload = serde_json::json!({
        "d": "integration-test.com",
        "n": "pageview",
        "u": "https://integration-test.com/landing?utm_source=google",
        "r": "https://www.google.com/search?q=test",
        "w": 1920
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .header("user-agent", "Mozilla/5.0 Chrome/120.0")
                .header("x-forwarded-for", "1.2.3.4")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    // Verify event is in the buffer
    assert_eq!(state.buffer.len(), 1);

    // Flush
    let flushed = state.buffer.flush().unwrap();
    assert_eq!(flushed, 1);
    assert!(state.buffer.is_empty());
}

#[tokio::test]
async fn test_ingest_with_all_fields() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let payload = serde_json::json!({
        "d": "full-test.com",
        "n": "purchase",
        "u": "https://full-test.com/checkout?utm_source=email&utm_medium=newsletter",
        "r": "https://t.co/abc123",
        "w": 375,
        "p": "{\"plan\":\"pro\",\"value\":99}",
        "ra": 99.99,
        "rc": "USD"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_ingest_validation_empty_domain() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let payload = serde_json::json!({
        "d": "",
        "n": "pageview",
        "u": "/"
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_ingest_validation_oversized_url() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let long_url = format!("https://example.com/{}", "a".repeat(3000));
    let payload = serde_json::json!({
        "d": "example.com",
        "n": "pageview",
        "u": long_url
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_stats_after_ingest() {
    let (state, _dir) = make_test_state();

    // Insert events directly into DB for testing
    {
        let conn = state.buffer.conn().lock();
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES ('test.com', 'v1', CURRENT_TIMESTAMP, 'pageview', '/')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES ('test.com', 'v2', CURRENT_TIMESTAMP, 'pageview', '/about')",
            [],
        )
        .unwrap();
    }

    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/main?site_id=test.com&period=30d")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let metrics: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(metrics["unique_visitors"], 2);
    assert_eq!(metrics["total_pageviews"], 2);
}

#[tokio::test]
async fn test_breakdown_after_ingest() {
    let (state, _dir) = make_test_state();

    {
        let conn = state.buffer.conn().lock();
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname, browser)
             VALUES ('test.com', 'v1', CURRENT_TIMESTAMP, 'pageview', '/', 'Chrome')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname, browser)
             VALUES ('test.com', 'v2', CURRENT_TIMESTAMP, 'pageview', '/', 'Firefox')",
            [],
        )
        .unwrap();
    }

    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/breakdown/browsers?site_id=test.com&period=30d")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 2);
}

#[tokio::test]
#[allow(clippy::significant_drop_tightening)]
async fn test_ua_parsing_populates_browser_os_fields() {
    let (state, dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let payload = serde_json::json!({
        "d": "ua-test.com",
        "n": "pageview",
        "u": "https://ua-test.com/",
    });

    let chrome_ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.6099.130 Safari/537.36";
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .header("user-agent", chrome_ua)
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    // Flush writes events to DuckDB, then to Parquet, then deletes from DuckDB.
    // Read from the Parquet files to verify browser/OS fields were stored.
    state.buffer.flush().unwrap();

    let (browser, browser_version, os, os_version) = {
        let conn = state.buffer.conn().lock();
        let glob = format!(
            "{}/site_id=ua-test.com/date=*/**.parquet",
            dir.path().display()
        );
        let sql =
            format!("SELECT browser, browser_version, os, os_version FROM read_parquet('{glob}')");
        let mut stmt = conn.prepare(&sql).unwrap();
        let mut rows = stmt.query([]).unwrap();
        let row = rows.next().unwrap().unwrap();
        let b: Option<String> = row.get(0).unwrap();
        let bv: Option<String> = row.get(1).unwrap();
        let o: Option<String> = row.get(2).unwrap();
        let ov: Option<String> = row.get(3).unwrap();
        (b, bv, o, ov)
    };

    assert_eq!(browser.as_deref(), Some("Chrome"));
    assert_eq!(browser_version.as_deref(), Some("120.0.6099.130"));
    assert_eq!(os.as_deref(), Some("Windows"));
    assert_eq!(os_version.as_deref(), Some("10.0"));
}

#[tokio::test]
#[allow(clippy::significant_drop_tightening)]
async fn test_ua_parsing_firefox_on_linux() {
    let (state, dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let payload = serde_json::json!({
        "d": "ua-test2.com",
        "n": "pageview",
        "u": "https://ua-test2.com/",
    });

    let firefox_ua = "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .header("user-agent", firefox_ua)
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    state.buffer.flush().unwrap();

    let (browser, os) = {
        let conn = state.buffer.conn().lock();
        let glob = format!(
            "{}/site_id=ua-test2.com/date=*/**.parquet",
            dir.path().display()
        );
        let sql = format!("SELECT browser, os FROM read_parquet('{glob}')");
        let mut stmt = conn.prepare(&sql).unwrap();
        let mut rows = stmt.query([]).unwrap();
        let row = rows.next().unwrap().unwrap();
        let b: Option<String> = row.get(0).unwrap();
        let o: Option<String> = row.get(1).unwrap();
        (b, o)
    };

    assert_eq!(browser.as_deref(), Some("Firefox"));
    assert_eq!(os.as_deref(), Some("Linux"));
}

fn make_test_state_with_sites(sites: Vec<String>) -> (Arc<AppState>, tempfile::TempDir) {
    let conn = Connection::open_in_memory().unwrap();
    schema::init_schema(&conn).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let storage = ParquetStorage::new(dir.path());
    let conn = Arc::new(Mutex::new(conn));
    let buffer = EventBuffer::new(1000, conn, storage);
    let state = Arc::new(AppState {
        buffer,
        secret: "test-secret-integration".to_string(),
        allowed_sites: sites,
    });
    (state, dir)
}

#[tokio::test]
async fn test_origin_validation_rejects_disallowed_origin() {
    let (state, _dir) = make_test_state_with_sites(vec!["allowed.com".to_string()]);
    let app = build_router(state);

    let payload = serde_json::json!({
        "d": "allowed.com",
        "n": "pageview",
        "u": "https://allowed.com/",
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .header("origin", "https://evil.com")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_origin_validation_allows_matching_origin() {
    let (state, _dir) = make_test_state_with_sites(vec!["allowed.com".to_string()]);
    let app = build_router(state);

    let payload = serde_json::json!({
        "d": "allowed.com",
        "n": "pageview",
        "u": "https://allowed.com/",
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .header("origin", "https://allowed.com")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_origin_validation_allows_no_origin_header() {
    let (state, _dir) = make_test_state_with_sites(vec!["allowed.com".to_string()]);
    let app = build_router(state);

    let payload = serde_json::json!({
        "d": "allowed.com",
        "n": "pageview",
        "u": "https://allowed.com/",
    });

    // No origin header — should be allowed (server-side requests)
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_health_check() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(&body[..], b"ok");
}
