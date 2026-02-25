use crate::api::auth;
use crate::api::stats;
use crate::dashboard;
use crate::ingest::handler::{ingest_event, AppState};
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::{header, HeaderValue, Method};
use axum::middleware;
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// Build the Axum router with all routes.
pub fn build_router(state: Arc<AppState>) -> Router {
    // Permissive CORS for ingestion (tracking script runs on any origin)
    let ingestion_cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::POST])
        .allow_headers([header::CONTENT_TYPE]);

    // Restrictive CORS for dashboard/stats/admin routes
    let dashboard_cors = build_dashboard_cors(state.dashboard_origin.as_deref());

    // Auth routes — always accessible (needed to log in)
    let auth_routes = Router::new()
        .route("/auth/setup", post(auth::auth_setup))
        .route("/auth/login", post(auth::auth_login))
        .route("/auth/logout", post(auth::auth_logout))
        .route("/auth/status", get(auth::auth_status));

    // API key management routes — require admin scope + CSRF protection
    let key_routes = Router::new()
        .route("/keys", post(auth::create_api_key))
        .route("/keys", get(auth::list_api_keys))
        .route("/keys/{key_hash}", delete(auth::revoke_api_key_handler))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_admin_auth,
        ));

    // Stats routes
    let stats_routes = Router::new()
        .route("/stats/main", get(stats::get_main_stats))
        .route("/stats/timeseries", get(stats::get_timeseries))
        .route("/stats/breakdown/pages", get(stats::get_pages_breakdown))
        .route(
            "/stats/breakdown/sources",
            get(stats::get_sources_breakdown),
        )
        .route(
            "/stats/breakdown/browsers",
            get(stats::get_browsers_breakdown),
        )
        .route("/stats/breakdown/os", get(stats::get_os_breakdown))
        .route(
            "/stats/breakdown/devices",
            get(stats::get_devices_breakdown),
        )
        .route(
            "/stats/breakdown/countries",
            get(stats::get_countries_breakdown),
        )
        .route("/stats/export", get(stats::get_export))
        .route("/stats/sessions", get(stats::get_sessions))
        .route("/stats/funnel", get(stats::get_funnel))
        .route("/stats/retention", get(stats::get_retention))
        .route("/stats/sequences", get(stats::get_sequences))
        .route("/stats/flow", get(stats::get_flow));

    // Protected routes — stats + key management, guarded by auth middleware
    let protected_routes = stats_routes
        .merge(key_routes)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_auth,
        ))
        .layer(dashboard_cors);

    // Ingestion with permissive CORS and 64 KB body limit (max valid event ~12 KB)
    let ingestion_routes = Router::new()
        .route("/event", post(ingest_event))
        .layer(DefaultBodyLimit::max(65_536))
        .layer(ingestion_cors);

    let api_routes = Router::new()
        .merge(ingestion_routes)
        .merge(auth_routes)
        .merge(protected_routes);

    Router::new()
        .route("/health", get(health_check))
        .route("/health/detailed", get(detailed_health_check))
        .route("/metrics", get(prometheus_metrics))
        .nest("/api", api_routes)
        .route("/", get(dashboard::serve_index))
        .route("/{*path}", get(dashboard::serve_asset))
        .layer(axum::middleware::map_response(add_security_headers))
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Inject OWASP-recommended security headers on every HTTP response.
async fn add_security_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    // Content-Security-Policy only on HTML responses (avoids breaking JSON APIs)
    let is_html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("text/html"));
    if is_html {
        headers.insert(
            "content-security-policy",
            HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'"),
        );
    }
    response
}

/// Build CORS layer for dashboard routes based on configured origin.
fn build_dashboard_cors(dashboard_origin: Option<&str>) -> CorsLayer {
    dashboard_origin.map_or_else(
        || {
            // No dashboard origin configured — allow all origins.
            // Set `dashboard_origin` in config to restrict cross-origin access.
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any)
        },
        |origin| {
            let allowed_origin = origin
                .parse::<axum::http::HeaderValue>()
                .unwrap_or_else(|_| axum::http::HeaderValue::from_static("*"));
            CorsLayer::new()
                .allow_origin(allowed_origin)
                .allow_methods([Method::GET, Method::POST, Method::DELETE])
                .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION, header::COOKIE])
                .allow_credentials(true)
        },
    )
}

/// GET /health — Simple health check endpoint.
async fn health_check() -> &'static str {
    "ok"
}

/// GET /health/detailed — Detailed health check with system info.
async fn detailed_health_check(
    State(state): State<Arc<AppState>>,
) -> axum::Json<serde_json::Value> {
    let buffered_events = state.buffer.len();
    let auth_configured = state.admin_password_hash.lock().is_some();
    let geoip_loaded = state.geoip.is_loaded();

    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "buffered_events": buffered_events,
        "auth_configured": auth_configured,
        "geoip_loaded": geoip_loaded,
        "filter_bots": state.filter_bots,
        "cache_entries": state.query_cache.len(),
        "cache_empty": state.query_cache.is_empty(),
    }))
}

/// GET /metrics — Prometheus-compatible metrics endpoint.
async fn prometheus_metrics(
    State(state): State<Arc<AppState>>,
) -> ([(header::HeaderName, &'static str); 1], String) {
    use std::fmt::Write;
    use std::sync::atomic::Ordering;

    let buffered = state.buffer.len();
    let cache_entries = state.query_cache.len();
    let auth_configured = u8::from(state.admin_password_hash.lock().is_some());
    let geoip_loaded = u8::from(state.geoip.is_loaded());
    let filter_bots = u8::from(state.filter_bots);
    let events_ingested = state.events_ingested_total.load(Ordering::Relaxed);

    let mut out = String::with_capacity(1024);
    let _ = writeln!(
        out,
        "# HELP mallard_buffered_events Number of events in the in-memory buffer"
    );
    let _ = writeln!(out, "# TYPE mallard_buffered_events gauge");
    let _ = writeln!(out, "mallard_buffered_events {buffered}");
    let _ = writeln!(
        out,
        "# HELP mallard_cache_entries Number of cached query results"
    );
    let _ = writeln!(out, "# TYPE mallard_cache_entries gauge");
    let _ = writeln!(out, "mallard_cache_entries {cache_entries}");
    let _ = writeln!(
        out,
        "# HELP mallard_auth_configured Whether admin password is set"
    );
    let _ = writeln!(out, "# TYPE mallard_auth_configured gauge");
    let _ = writeln!(out, "mallard_auth_configured {auth_configured}");
    let _ = writeln!(
        out,
        "# HELP mallard_geoip_loaded Whether GeoIP database is loaded"
    );
    let _ = writeln!(out, "# TYPE mallard_geoip_loaded gauge");
    let _ = writeln!(out, "mallard_geoip_loaded {geoip_loaded}");
    let _ = writeln!(
        out,
        "# HELP mallard_filter_bots Whether bot filtering is enabled"
    );
    let _ = writeln!(out, "# TYPE mallard_filter_bots gauge");
    let _ = writeln!(out, "mallard_filter_bots {filter_bots}");
    let _ = writeln!(
        out,
        "# HELP mallard_events_ingested_total Total events successfully buffered since startup"
    );
    let _ = writeln!(out, "# TYPE mallard_events_ingested_total counter");
    let _ = writeln!(out, "mallard_events_ingested_total {events_ingested}");

    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::auth::{ApiKeyStore, SessionStore};
    use crate::ingest::buffer::EventBuffer;
    use crate::storage::parquet::ParquetStorage;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use duckdb::Connection;
    use http_body_util::BodyExt;
    use parking_lot::Mutex;
    use tower::ServiceExt;

    fn make_test_state() -> (Arc<AppState>, tempfile::TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let dir = tempfile::tempdir().unwrap();
        crate::storage::schema::setup_query_view(&conn, dir.path()).unwrap();
        let storage = ParquetStorage::new(dir.path());
        let conn = Arc::new(Mutex::new(conn));
        let buffer = EventBuffer::new(1000, conn, storage);
        let state = Arc::new(AppState {
            buffer,
            secret: "test-secret".to_string(),
            allowed_sites: Vec::new(),
            geoip: crate::ingest::geoip::GeoIpReader::open(None),
            filter_bots: false,
            sessions: SessionStore::new(3600),
            api_keys: ApiKeyStore::new(),
            admin_password_hash: Mutex::new(None),
            dashboard_origin: None,
            query_cache: crate::query::cache::QueryCache::new(0),
            rate_limiter: crate::ingest::ratelimit::RateLimiter::new(0),
            login_attempt_tracker: crate::api::auth::LoginAttemptTracker::new(0, 300),
            events_ingested_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        });
        (state, dir)
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

    #[tokio::test]
    async fn test_prometheus_metrics() {
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
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.contains("text/plain"));
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("mallard_buffered_events 0"));
        assert!(text.contains("mallard_cache_entries 0"));
        assert!(text.contains("mallard_auth_configured 0"));
        assert!(text.contains("mallard_geoip_loaded 0"));
        assert!(text.contains("mallard_filter_bots 0"));
    }

    #[tokio::test]
    async fn test_ingest_event() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

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
    async fn test_ingest_event_invalid_payload() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/event")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Missing required fields should return 422 (Unprocessable Entity from Axum's Json extractor)
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn test_ingest_event_empty_fields() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

        let payload = serde_json::json!({
            "d": "",
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

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_stats_main_empty() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

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
    }

    #[tokio::test]
    async fn test_dashboard_index() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_detailed_health_check() {
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
    async fn test_not_found() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nonexistent.file")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_cors_headers() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/api/event")
                    .header("origin", "https://example.com")
                    .header("access-control-request-method", "POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(response
            .headers()
            .contains_key("access-control-allow-origin"));
    }
}
