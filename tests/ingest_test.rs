use axum::body::Body;
use axum::http::{Request, StatusCode};
use duckdb::Connection;
use http_body_util::BodyExt;
use mallard_metrics::api::auth::{ApiKeyStore, SessionStore};
use mallard_metrics::ingest::buffer::EventBuffer;
use mallard_metrics::ingest::geoip::GeoIpReader;
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
    schema::setup_query_view(&conn, dir.path()).unwrap();
    let storage = ParquetStorage::new(dir.path());
    let conn = Arc::new(Mutex::new(conn));
    let buffer = EventBuffer::new(1000, conn, storage);
    let state = Arc::new(AppState {
        buffer,
        secret: "test-secret-integration".to_string(),
        allowed_sites: Vec::new(),
        geoip: GeoIpReader::open(None),
        filter_bots: false,
        sessions: SessionStore::new(3600),
        api_keys: ApiKeyStore::new(),
        admin_password_hash: Mutex::new(None),
        dashboard_origin: None,
        query_cache: mallard_metrics::query::cache::QueryCache::new(0),
        rate_limiter: mallard_metrics::ingest::ratelimit::RateLimiter::new(0),
        login_attempt_tracker: mallard_metrics::api::auth::LoginAttemptTracker::new(0, 300),
        events_ingested_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
    schema::setup_query_view(&conn, dir.path()).unwrap();
    let storage = ParquetStorage::new(dir.path());
    let conn = Arc::new(Mutex::new(conn));
    let buffer = EventBuffer::new(1000, conn, storage);
    let state = Arc::new(AppState {
        buffer,
        secret: "test-secret-integration".to_string(),
        allowed_sites: sites,
        geoip: GeoIpReader::open(None),
        filter_bots: false,
        sessions: SessionStore::new(3600),
        api_keys: ApiKeyStore::new(),
        admin_password_hash: Mutex::new(None),
        dashboard_origin: None,
        query_cache: mallard_metrics::query::cache::QueryCache::new(0),
        rate_limiter: mallard_metrics::ingest::ratelimit::RateLimiter::new(0),
        login_attempt_tracker: mallard_metrics::api::auth::LoginAttemptTracker::new(0, 300),
        events_ingested_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
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

// --- Phase 3: Behavioral Analytics API integration tests ---

#[tokio::test]
async fn test_sessions_endpoint_returns_ok() {
    let (state, _dir) = make_test_state();
    {
        let conn = state.buffer.conn().lock();
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES ('test.com', 'v1', CURRENT_TIMESTAMP, 'pageview', '/')",
            [],
        )
        .unwrap();
    }

    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/sessions?site_id=test.com&period=30d")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Returns 200 OK with graceful degradation even without behavioral extension
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Should have session metric fields
    assert!(json.get("total_sessions").is_some());
    assert!(json.get("avg_session_duration_secs").is_some());
    assert!(json.get("avg_pages_per_session").is_some());
}

#[tokio::test]
async fn test_funnel_endpoint_with_valid_steps() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/funnel?site_id=test.com&period=30d&steps=page%3A%2F%2Cevent%3Asignup&window=1%20day")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Returns 200 OK with graceful degradation
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_funnel_endpoint_rejects_invalid_steps() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/funnel?site_id=test.com&period=30d&steps=DROP%20TABLE%20events&window=1%20day")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_funnel_endpoint_rejects_invalid_window() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/funnel?site_id=test.com&period=30d&steps=page%3A%2F&window=1%20day%3B%20DROP%20TABLE")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_retention_endpoint_returns_ok() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/retention?site_id=test.com&period=90d&weeks=4")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_retention_endpoint_rejects_invalid_weeks() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/retention?site_id=test.com&period=90d&weeks=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_sequences_endpoint_returns_ok() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/sequences?site_id=test.com&period=30d&steps=page%3A%2F%2Cevent%3Asignup")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.get("converting_visitors").is_some());
    assert!(json.get("total_visitors").is_some());
    assert!(json.get("conversion_rate").is_some());
}

#[tokio::test]
async fn test_sequences_endpoint_requires_two_steps() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/sequences?site_id=test.com&period=30d&steps=page%3A%2F")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_flow_endpoint_returns_ok() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/flow?site_id=test.com&period=30d&page=/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_flow_endpoint_rejects_empty_page() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/flow?site_id=test.com&period=30d&page=")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// --- Phase 4.2: Authentication integration tests ---

fn make_test_state_with_password(password: &str) -> (Arc<AppState>, tempfile::TempDir) {
    let conn = Connection::open_in_memory().unwrap();
    schema::init_schema(&conn).unwrap();
    let dir = tempfile::tempdir().unwrap();
    schema::setup_query_view(&conn, dir.path()).unwrap();
    let storage = ParquetStorage::new(dir.path());
    let conn = Arc::new(Mutex::new(conn));
    let buffer = EventBuffer::new(1000, conn, storage);
    let hash = mallard_metrics::api::auth::hash_password(password).unwrap();
    let state = Arc::new(AppState {
        buffer,
        secret: "test-secret-integration".to_string(),
        allowed_sites: Vec::new(),
        geoip: GeoIpReader::open(None),
        filter_bots: false,
        sessions: SessionStore::new(3600),
        api_keys: ApiKeyStore::new(),
        admin_password_hash: Mutex::new(Some(hash)),
        dashboard_origin: None,
        query_cache: mallard_metrics::query::cache::QueryCache::new(0),
        rate_limiter: mallard_metrics::ingest::ratelimit::RateLimiter::new(0),
        login_attempt_tracker: mallard_metrics::api::auth::LoginAttemptTracker::new(0, 300),
        events_ingested_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    });
    (state, dir)
}

#[tokio::test]
async fn test_auth_status_no_password_configured() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/auth/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["setup_required"], true);
    assert_eq!(json["authenticated"], true); // Open access when no password
}

#[tokio::test]
async fn test_auth_setup_creates_password() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let payload = serde_json::json!({ "password": "secure-password-123" });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/setup")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Verify Set-Cookie header is present
    let cookie = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cookie.contains("mm_session="));
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));

    // Verify password is now configured
    assert!(state.admin_password_hash.lock().is_some());
}

#[tokio::test]
async fn test_auth_setup_rejects_short_password() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let payload = serde_json::json!({ "password": "short" });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/setup")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_auth_setup_rejects_second_setup() {
    let (state, _dir) = make_test_state_with_password("existing-password");
    let app = build_router(state);

    let payload = serde_json::json!({ "password": "new-password-123" });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/setup")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_auth_login_success() {
    let (state, _dir) = make_test_state_with_password("my-secure-password");
    let app = build_router(state);

    let payload = serde_json::json!({ "password": "my-secure-password" });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(cookie.contains("mm_session="));
}

#[tokio::test]
async fn test_auth_login_wrong_password() {
    let (state, _dir) = make_test_state_with_password("correct-password");
    let app = build_router(state);

    let payload = serde_json::json!({ "password": "wrong-password" });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_auth_middleware_blocks_unauthenticated_stats() {
    let (state, _dir) = make_test_state_with_password("admin-password");
    let app = build_router(state);

    // Stats route without session cookie should return 401
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/main?site_id=test.com&period=30d")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_auth_middleware_allows_authenticated_stats() {
    let (state, _dir) = make_test_state_with_password("admin-password");

    // Create a session directly
    let token = state.sessions.create_session("admin");

    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/main?site_id=test.com&period=30d")
                .header("cookie", format!("mm_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_auth_middleware_allows_api_key() {
    let (state, _dir) = make_test_state_with_password("admin-password");

    // Add an API key
    let key = mallard_metrics::api::auth::generate_api_key();
    state.api_keys.add_key(
        "test",
        &key,
        mallard_metrics::api::auth::ApiKeyScope::ReadOnly,
    );

    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/main?site_id=test.com&period=30d")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_auth_middleware_allows_event_ingestion_without_auth() {
    let (state, _dir) = make_test_state_with_password("admin-password");
    let app = build_router(state);

    // Event ingestion should work even when auth is configured
    let payload = serde_json::json!({
        "d": "example.com",
        "n": "pageview",
        "u": "https://example.com/",
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
async fn test_auth_logout() {
    let (state, _dir) = make_test_state_with_password("admin-password");
    let token = state.sessions.create_session("admin");

    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/logout")
                .header("cookie", format!("mm_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Session should be invalidated
    assert!(state.sessions.validate_session(&token).is_none());
}

#[tokio::test]
async fn test_auth_status_with_password_configured() {
    let (state, _dir) = make_test_state_with_password("admin-password");
    let token = state.sessions.create_session("admin");

    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/auth/status")
                .header("cookie", format!("mm_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["setup_required"], false);
    assert_eq!(json["authenticated"], true);
}

// --- Phase 4.4: API Key Management integration tests ---

#[tokio::test]
async fn test_create_api_key() {
    let (state, _dir) = make_test_state_with_password("admin-password");
    let token = state.sessions.create_session("admin");

    let app = build_router(Arc::clone(&state));
    let payload = serde_json::json!({ "name": "test-key", "scope": "ReadOnly" });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/keys")
                .header("content-type", "application/json")
                .header("cookie", format!("mm_session={token}"))
                .body(Body::from(serde_json::to_string(&payload).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["key"].as_str().unwrap().starts_with("mm_"));
    assert_eq!(json["name"], "test-key");
    assert_eq!(json["scope"], "ReadOnly");
}

#[tokio::test]
async fn test_list_api_keys() {
    let (state, _dir) = make_test_state_with_password("admin-password");
    let token = state.sessions.create_session("admin");

    // Create a key
    let key = mallard_metrics::api::auth::generate_api_key();
    state.api_keys.add_key(
        "my-key",
        &key,
        mallard_metrics::api::auth::ApiKeyScope::Admin,
    );

    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/keys")
                .header("cookie", format!("mm_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(json.len(), 1);
    assert_eq!(json[0]["name"], "my-key");
    assert_eq!(json[0]["scope"], "Admin");
}

#[tokio::test]
async fn test_revoke_api_key() {
    let (state, _dir) = make_test_state_with_password("admin-password");
    let token = state.sessions.create_session("admin");

    let key = mallard_metrics::api::auth::generate_api_key();
    let key_hash = state.api_keys.add_key(
        "revoke-me",
        &key,
        mallard_metrics::api::auth::ApiKeyScope::ReadOnly,
    );

    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/keys/{key_hash}"))
                .header("cookie", format!("mm_session={token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Key should now be revoked
    assert!(state.api_keys.validate_key(&key).is_none());
}

#[tokio::test]
async fn test_api_key_endpoints_require_auth() {
    let (state, _dir) = make_test_state_with_password("admin-password");
    let app = build_router(state);

    // Unauthenticated list should fail
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/keys")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// --- Phase 5: Operational Excellence integration tests ---

#[tokio::test]
async fn test_detailed_health_check_endpoint() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health/detailed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "ok");
    assert!(json.get("version").is_some());
    assert_eq!(json["buffered_events"], 0);
    assert_eq!(json["auth_configured"], false);
    assert_eq!(json["geoip_loaded"], false);
    assert_eq!(json["filter_bots"], false);
}

#[tokio::test]
async fn test_export_csv_format() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/export?site_id=test.com&period=7d&format=csv")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_type, "text/csv");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.starts_with("date,visitors,pageviews,top_page,top_source"));
}

#[tokio::test]
async fn test_export_json_format() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/export?site_id=test.com&period=7d&format=json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap();
    assert_eq!(content_type, "application/json");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json.is_array());
}

#[tokio::test]
async fn test_rate_limiting() {
    // Create state with rate limit of 2 per second
    let conn = Connection::open_in_memory().unwrap();
    schema::init_schema(&conn).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let storage = ParquetStorage::new(dir.path());
    let conn = Arc::new(Mutex::new(conn));
    let buffer = EventBuffer::new(1000, conn, storage);
    let state = Arc::new(AppState {
        buffer,
        secret: "test-secret".to_string(),
        allowed_sites: Vec::new(),
        geoip: GeoIpReader::open(None),
        filter_bots: false,
        sessions: SessionStore::new(3600),
        api_keys: ApiKeyStore::new(),
        admin_password_hash: Mutex::new(None),
        dashboard_origin: None,
        query_cache: mallard_metrics::query::cache::QueryCache::new(0),
        rate_limiter: mallard_metrics::ingest::ratelimit::RateLimiter::new(2),
        login_attempt_tracker: mallard_metrics::api::auth::LoginAttemptTracker::new(0, 300),
        events_ingested_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    });

    let payload = serde_json::json!({
        "d": "example.com",
        "n": "pageview",
        "u": "https://example.com/",
    });
    let body_str = serde_json::to_string(&payload).unwrap();

    // First two requests should succeed
    for _ in 0..2 {
        let app = build_router(Arc::clone(&state));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/event")
                    .header("content-type", "application/json")
                    .body(Body::from(body_str.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
    }

    // Third request should be rate limited
    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .body(Body::from(body_str))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_detailed_health_check_with_auth() {
    let (state, _dir) = make_test_state_with_password("admin-pass");
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health/detailed")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["auth_configured"], true);
}

// --- New integration tests for production-readiness gaps ---

/// Make a state with password AND brute-force protection enabled (max 3 attempts, 300s lockout).
fn make_test_state_with_lockout(password: &str) -> (Arc<AppState>, tempfile::TempDir) {
    let conn = Connection::open_in_memory().unwrap();
    schema::init_schema(&conn).unwrap();
    let dir = tempfile::tempdir().unwrap();
    schema::setup_query_view(&conn, dir.path()).unwrap();
    let storage = ParquetStorage::new(dir.path());
    let conn = Arc::new(Mutex::new(conn));
    let buffer = EventBuffer::new(1000, conn, storage);
    let hash = mallard_metrics::api::auth::hash_password(password).unwrap();
    let state = Arc::new(AppState {
        buffer,
        secret: "test-secret".to_string(),
        allowed_sites: Vec::new(),
        geoip: GeoIpReader::open(None),
        filter_bots: false,
        sessions: SessionStore::new(3600),
        api_keys: ApiKeyStore::new(),
        admin_password_hash: Mutex::new(Some(hash)),
        dashboard_origin: None,
        query_cache: mallard_metrics::query::cache::QueryCache::new(0),
        rate_limiter: mallard_metrics::ingest::ratelimit::RateLimiter::new(0),
        login_attempt_tracker: mallard_metrics::api::auth::LoginAttemptTracker::new(3, 300),
        events_ingested_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    });
    (state, dir)
}

#[tokio::test]
async fn test_login_rate_limited_after_failures() {
    let (state, _dir) = make_test_state_with_lockout("correct-password");

    // Exhaust 3 attempts with wrong password
    for _ in 0..3 {
        let app = build_router(Arc::clone(&state));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .header("x-forwarded-for", "10.0.0.5")
                    .body(Body::from(r#"{"password":"wrong"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // 4th attempt (even with correct password) should be 429
    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .header("x-forwarded-for", "10.0.0.5")
                .body(Body::from(r#"{"password":"correct-password"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn test_login_success_clears_failure_count() {
    let (state, _dir) = make_test_state_with_lockout("my-password");

    // 2 wrong attempts (below lockout threshold of 3)
    for _ in 0..2 {
        let app = build_router(Arc::clone(&state));
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .header("x-forwarded-for", "10.0.0.6")
                .body(Body::from(r#"{"password":"wrong"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    }

    // Successful login clears the failure count
    let app = build_router(Arc::clone(&state));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .header("x-forwarded-for", "10.0.0.6")
                .body(Body::from(r#"{"password":"my-password"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_ingest_rejects_oversized_body() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    // Build a payload larger than 64 KB
    let big_props = "x".repeat(70_000);
    let payload = format!(
        r#"{{"d":"example.com","n":"pageview","u":"https://example.com/","p":{big_props:?}}}"#
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .body(Body::from(payload))
                .unwrap(),
        )
        .await
        .unwrap();

    // Axum should reject with 413 Payload Too Large
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_security_headers_present() {
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
    assert_eq!(
        response.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
    assert!(response.headers().contains_key("referrer-policy"));
}

#[tokio::test]
async fn test_events_ingested_counter() {
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    let payload = serde_json::json!({
        "d": "counter-test.com",
        "n": "pageview",
        "u": "https://counter-test.com/",
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
    assert_eq!(
        state
            .events_ingested_total
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn test_prometheus_metrics_includes_counter() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("mallard_events_ingested_total"),
        "Prometheus output must include events counter"
    );
}

#[tokio::test]
async fn test_api_key_scope_readonly_cannot_create_key() {
    let (state, _dir) = make_test_state_with_password("admin-password");

    // Add a ReadOnly API key
    let ro_key = mallard_metrics::api::auth::generate_api_key();
    state.api_keys.add_key(
        "read-only",
        &ro_key,
        mallard_metrics::api::auth::ApiKeyScope::ReadOnly,
    );

    let app = build_router(state);

    // Attempt to create a new key with the ReadOnly key — should be 403
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/keys")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {ro_key}"))
                .body(Body::from(r#"{"name":"new-key","scope":"Admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_api_key_scope_admin_can_create_key() {
    let (state, _dir) = make_test_state_with_password("admin-password");

    // Add an Admin API key
    let admin_key = mallard_metrics::api::auth::generate_api_key();
    state.api_keys.add_key(
        "admin",
        &admin_key,
        mallard_metrics::api::auth::ApiKeyScope::Admin,
    );

    let app = build_router(state);

    // Admin key should succeed on key creation
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/keys")
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {admin_key}"))
                .body(Body::from(r#"{"name":"new-key","scope":"ReadOnly"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_x_api_key_header_authentication() {
    let (state, _dir) = make_test_state_with_password("admin-password");

    // Grant stats access via API key (Admin)
    let key = mallard_metrics::api::auth::generate_api_key();
    state.api_keys.add_key(
        "test-key",
        &key,
        mallard_metrics::api::auth::ApiKeyScope::Admin,
    );

    let app = build_router(state);

    // Use X-API-Key header instead of Authorization: Bearer
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/main?site_id=example.com&period=30d")
                .header("x-api-key", &key)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_stats_invalid_site_id_rejected() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    // site_id with slash should be rejected
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/main?site_id=example.com/path&period=30d")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_stats_empty_site_id_rejected() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    // empty site_id should be rejected
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/main?site_id=&period=30d")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_timeseries_invalid_period_rejected() {
    let (state, _dir) = make_test_state();
    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/timeseries?site_id=example.com&period=bogus")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_data_persists_after_view_rebuild() {
    // Ingest events, flush to Parquet, rebuild the events_all view, verify queries still work
    let (state, dir) = make_test_state();

    // Insert and flush an event
    {
        let conn = state.buffer.conn().lock();
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES ('persist-test.com', 'v1', CURRENT_TIMESTAMP, 'pageview', '/')",
            [],
        )
        .unwrap();
    }
    // Flush to Parquet
    state.buffer.flush().unwrap();

    // Rebuild the events_all view (simulates a server restart)
    {
        let conn = state.buffer.conn().lock();
        mallard_metrics::storage::schema::setup_query_view(&conn, dir.path()).unwrap();
    }

    let app = build_router(Arc::clone(&state));

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/main?site_id=persist-test.com&period=30d")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let metrics: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // After view rebuild, the flushed data should be visible
    assert!(
        metrics["total_pageviews"].as_u64().unwrap_or(0) >= 1,
        "Flushed events should be visible after view rebuild: {metrics}"
    );
}

#[tokio::test]
async fn test_login_lockout_respects_ip_isolation() {
    // IP-A exhausts its attempts; IP-B must remain unaffected.
    let (state, _dir) = make_test_state_with_lockout("secret-pass");

    // Exhaust 3 attempts from IP-A with a wrong password
    for _ in 0..3 {
        let app = build_router(Arc::clone(&state));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .header("x-forwarded-for", "10.1.2.3")
                    .body(Body::from(r#"{"password":"wrong"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // IP-A is now locked out
    let app = build_router(Arc::clone(&state));
    let locked = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .header("x-forwarded-for", "10.1.2.3")
                .body(Body::from(r#"{"password":"secret-pass"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(locked.status(), StatusCode::TOO_MANY_REQUESTS);

    // IP-B has a clean slate and should authenticate successfully
    let app = build_router(Arc::clone(&state));
    let ok = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .header("x-forwarded-for", "10.9.9.9")
                .body(Body::from(r#"{"password":"secret-pass"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_security_headers_on_html_response() {
    // CSP should appear on HTML (dashboard) responses but NOT on JSON API responses.
    let (state, _dir) = make_test_state();
    let app = build_router(Arc::clone(&state));

    // The dashboard index returns text/html — CSP must be present
    let html_response = app
        .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert!(
        html_response
            .headers()
            .contains_key("content-security-policy"),
        "CSP must be present on HTML response"
    );

    // A JSON API endpoint must NOT carry CSP
    let app = build_router(Arc::clone(&state));
    let json_response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        !json_response
            .headers()
            .contains_key("content-security-policy"),
        "CSP must NOT be present on non-HTML JSON response"
    );
}

#[tokio::test]
async fn test_csrf_blocks_session_auth_key_creation() {
    // When dashboard_origin is configured, a session-authenticated POST /api/keys
    // without a matching Origin header must receive 403.
    let conn = Connection::open_in_memory().unwrap();
    schema::init_schema(&conn).unwrap();
    let dir = tempfile::tempdir().unwrap();
    schema::setup_query_view(&conn, dir.path()).unwrap();
    let storage = ParquetStorage::new(dir.path());
    let conn = Arc::new(Mutex::new(conn));
    let buffer = EventBuffer::new(1000, conn, storage);
    let hash = mallard_metrics::api::auth::hash_password("csrf-test-pass").unwrap();
    let state = Arc::new(AppState {
        buffer,
        secret: "test-secret".to_string(),
        allowed_sites: Vec::new(),
        geoip: GeoIpReader::open(None),
        filter_bots: false,
        sessions: SessionStore::new(3600),
        api_keys: ApiKeyStore::new(),
        admin_password_hash: Mutex::new(Some(hash)),
        dashboard_origin: Some("https://analytics.example.com".to_string()),
        query_cache: mallard_metrics::query::cache::QueryCache::new(0),
        rate_limiter: mallard_metrics::ingest::ratelimit::RateLimiter::new(0),
        login_attempt_tracker: mallard_metrics::api::auth::LoginAttemptTracker::new(0, 300),
        events_ingested_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
    });

    // Create a valid session directly (bypasses login)
    let token = state.sessions.create_session("admin");

    let app = build_router(Arc::clone(&state));

    // POST /api/keys with session cookie but wrong Origin — expect 403
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/keys")
                .header("content-type", "application/json")
                .header("cookie", format!("mm_session={token}"))
                .header("origin", "https://evil.example.org")
                .body(Body::from(r#"{"name":"bad-key","scope":"ReadOnly"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_x_api_key_readonly_blocks_key_management() {
    // A ReadOnly key passed via X-API-Key header must be rejected on admin routes.
    let (state, _dir) = make_test_state_with_password("admin-password");

    let ro_key = mallard_metrics::api::auth::generate_api_key();
    state.api_keys.add_key(
        "readonly-via-header",
        &ro_key,
        mallard_metrics::api::auth::ApiKeyScope::ReadOnly,
    );

    let app = build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/keys")
                .header("content-type", "application/json")
                .header("x-api-key", &ro_key)
                .body(Body::from(r#"{"name":"sneaky","scope":"Admin"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
