use crate::api::auth;
use crate::api::stats;
use crate::dashboard;
use crate::ingest::handler::{ingest_event, AppState};
use axum::extract::DefaultBodyLimit;
use axum::extract::State;
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::Instrument;

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

    // Ingestion with permissive CORS and 64 KB body limit (max valid event ~12 KB).
    // GET /api/event is included for pixel / <img> tracker compatibility.
    let ingestion_routes = Router::new()
        .route("/event", post(ingest_event))
        .route("/event", get(pixel_track))
        .layer(DefaultBodyLimit::max(65_536))
        .layer(ingestion_cors);

    let api_routes = Router::new()
        .merge(ingestion_routes)
        .merge(auth_routes)
        .merge(protected_routes);

    Router::new()
        .route("/health", get(health_check))
        .route("/health/ready", get(readiness_check))
        .route("/health/detailed", get(detailed_health_check))
        .route("/metrics", get(prometheus_metrics))
        .route("/robots.txt", get(robots_txt))
        .route("/.well-known/security.txt", get(security_txt))
        .nest("/api", api_routes)
        .route("/", get(dashboard::serve_index))
        .route("/{*path}", get(dashboard::serve_asset))
        .layer(middleware::from_fn(request_id_middleware))
        .layer(axum::middleware::map_response(add_security_headers))
        .layer(CompressionLayer::new())
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            std::time::Duration::from_secs(30),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Inject OWASP-recommended security headers and Cache-Control on every HTTP response.
async fn add_security_headers(mut response: Response) -> Response {
    // Snapshot status BEFORE taking a mutable reference to headers so both
    // borrows do not coexist (the borrow checker forbids mixed &/&mut on the
    // same value through different fields when they alias through the struct).
    let status = response.status();
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
    headers.insert(
        "permissions-policy",
        HeaderValue::from_static("geolocation=(), microphone=(), camera=()"),
    );
    // HSTS: instruct browsers to enforce HTTPS for 1 year.
    // Safe to include on HTTP deployments — browsers only process this header
    // when received over HTTPS, so it is a no-op on plain HTTP.
    headers.insert(
        "strict-transport-security",
        HeaderValue::from_static("max-age=31536000; includeSubDomains"),
    );
    // Add Retry-After: 1 to any 429 response that does not already carry the
    // header (the login endpoint sets its own value based on the lockout period).
    if status == StatusCode::TOO_MANY_REQUESTS && !headers.contains_key("retry-after") {
        headers.insert("retry-after", HeaderValue::from_static("1"));
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let is_html = content_type.contains("text/html");
    let is_json = content_type.contains("application/json");

    if is_html {
        headers.insert(
            "content-security-policy",
            HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'"),
        );
    }

    // Prevent CDN/browser caches from serving stale or cross-user analytics data.
    if is_json {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, no-cache"),
        );
    }

    response
}

/// Middleware that assigns an X-Request-ID to every request and records it in
/// the tracing span so that all log lines emitted during request processing
/// carry the same `request_id` field.
///
/// If the upstream proxy already set an `X-Request-ID` header, the existing
/// value is used (enabling end-to-end correlation through the proxy tier).
async fn request_id_middleware(request: Request<axum::body::Body>, next: Next) -> Response {
    let id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), String::from);

    // Run the handler inside a span that carries request_id so all log events
    // emitted during processing are correlated to this request.
    let span = tracing::info_span!("http_request", request_id = %id);
    let mut response = next.run(request).instrument(span).await;
    if let Ok(val) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", val);
    }
    response
}

/// GET /robots.txt — Prevent search engines from indexing the dashboard or API.
async fn robots_txt() -> impl axum::response::IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "User-agent: *\nDisallow: /api/\nDisallow: /health\nDisallow: /metrics\n",
    )
}

/// GET /.well-known/security.txt — RFC 9116 security vulnerability reporting policy.
async fn security_txt() -> impl axum::response::IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "# Mallard Metrics security policy\n\
         # To report a vulnerability, open an issue at:\n\
         # https://github.com/mallard-metrics/mallard-metrics/issues\n\
         Contact: mailto:security@mallard-metrics.example\n\
         Expires: 2027-01-01T00:00:00.000Z\n\
         Preferred-Languages: en\n",
    )
}

/// 1×1 transparent GIF (43 bytes) used by the pixel tracking endpoint.
///
/// Defined at module level to avoid the `items_after_statements` clippy lint
/// that fires when a `const` is declared after an `await` expression inside a
/// function body.
const TRANSPARENT_GIF_1X1: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff, 0xff,
    0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// GET /api/event — Pixel / `<img>` tracker compatibility endpoint.
///
/// Accepts the same core parameters as `POST /api/event` but via query string,
/// returning a 1×1 transparent GIF so that it can be embedded as an `<img>`
/// tag in HTML emails and other contexts where JavaScript is unavailable.
///
/// Revenue and custom-property fields are deliberately excluded because they
/// cannot be validated or sanitised reliably in a plain query string.
async fn pixel_track(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<crate::ingest::handler::PixelParams>,
) -> impl axum::response::IntoResponse {
    // Reuse the shared event processing helper; ignore the result (fire-and-forget).
    crate::ingest::handler::process_pixel_event(&state, &headers, params).await;

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "image/gif")],
        TRANSPARENT_GIF_1X1,
    )
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

/// GET /health — Simple liveness probe. Always returns "ok" if the process is alive.
async fn health_check() -> &'static str {
    "ok"
}

/// GET /health/ready — Readiness probe.
///
/// Returns 200 when the DuckDB connection is alive and the events_all view is
/// queryable.  Returns 503 if the database is not reachable, so Kubernetes can
/// hold traffic until the instance is ready.
async fn readiness_check(State(state): State<Arc<AppState>>) -> Response {
    let ok = tokio::task::spawn_blocking(move || {
        let conn = state.buffer.conn().lock();
        conn.execute_batch("SELECT 1 FROM events_all LIMIT 0")
            .is_ok()
    })
    .await
    .unwrap_or(false);

    if ok {
        axum::response::IntoResponse::into_response((StatusCode::OK, "ready"))
    } else {
        axum::response::IntoResponse::into_response((
            StatusCode::SERVICE_UNAVAILABLE,
            "database not ready",
        ))
    }
}

/// GET /health/detailed — Detailed health check with system info.
async fn detailed_health_check(
    State(state): State<Arc<AppState>>,
) -> axum::Json<serde_json::Value> {
    let buffered_events = state.buffer.len();
    let buffer_empty = state.buffer.is_empty();
    let auth_configured = state.admin_password_hash.lock().is_some();
    let geoip_loaded = state.geoip.is_loaded();

    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "buffered_events": buffered_events,
        "buffer_empty": buffer_empty,
        "auth_configured": auth_configured,
        "geoip_loaded": geoip_loaded,
        "filter_bots": state.filter_bots,
        "cache_entries": state.query_cache.len(),
        "cache_empty": state.query_cache.is_empty(),
    }))
}

/// GET /metrics — Prometheus-compatible metrics endpoint.
///
/// If MALLARD_METRICS_TOKEN is set at startup, requires Authorization: Bearer <token>.
fn build_metrics_body(state: &AppState) -> String {
    use std::fmt::Write;
    use std::sync::atomic::Ordering;

    let buffered = state.buffer.len();
    let cache_entries = state.query_cache.len();
    let auth_configured = u8::from(state.admin_password_hash.lock().is_some());
    let geoip_loaded = u8::from(state.geoip.is_loaded());
    let filter_bots = u8::from(state.filter_bots);
    let events_ingested = state.events_ingested_total.load(Ordering::Relaxed);
    let flush_failures = state.flush_failures_total.load(Ordering::Relaxed);
    let rate_limit_rejections = state.rate_limit_rejections_total.load(Ordering::Relaxed);
    let login_failures = state.login_failures_total.load(Ordering::Relaxed);
    let cache_hits = state.query_cache.hits.load(Ordering::Relaxed);
    let cache_misses = state.query_cache.misses.load(Ordering::Relaxed);

    let mut out = String::with_capacity(2048);
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
    let _ = writeln!(
        out,
        "# HELP mallard_flush_failures_total Total Parquet flush failures since startup"
    );
    let _ = writeln!(out, "# TYPE mallard_flush_failures_total counter");
    let _ = writeln!(out, "mallard_flush_failures_total {flush_failures}");
    let _ = writeln!(
        out,
        "# HELP mallard_rate_limit_rejections_total Total ingest requests rejected by rate limiter"
    );
    let _ = writeln!(out, "# TYPE mallard_rate_limit_rejections_total counter");
    let _ = writeln!(
        out,
        "mallard_rate_limit_rejections_total {rate_limit_rejections}"
    );
    let _ = writeln!(
        out,
        "# HELP mallard_login_failures_total Total failed login attempts since startup"
    );
    let _ = writeln!(out, "# TYPE mallard_login_failures_total counter");
    let _ = writeln!(out, "mallard_login_failures_total {login_failures}");
    let _ = writeln!(
        out,
        "# HELP mallard_cache_hits_total Total query cache hits since startup"
    );
    let _ = writeln!(out, "# TYPE mallard_cache_hits_total counter");
    let _ = writeln!(out, "mallard_cache_hits_total {cache_hits}");
    let _ = writeln!(
        out,
        "# HELP mallard_cache_misses_total Total query cache misses since startup"
    );
    let _ = writeln!(out, "# TYPE mallard_cache_misses_total counter");
    let _ = writeln!(out, "mallard_cache_misses_total {cache_misses}");

    out
}

/// GET /metrics — Prometheus-compatible metrics endpoint.
///
/// If MALLARD_METRICS_TOKEN is set at startup, requires Authorization: Bearer <token>.
async fn prometheus_metrics(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Response {
    // Optional bearer-token guard for the metrics endpoint.
    if let Some(expected) = &state.metrics_token {
        let authorized = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .is_some_and(|token| {
                crate::api::auth::constant_time_eq(token.as_bytes(), expected.as_bytes())
            });
        if !authorized {
            return axum::response::IntoResponse::into_response((
                StatusCode::UNAUTHORIZED,
                "Unauthorized",
            ));
        }
    }

    axum::response::IntoResponse::into_response((
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        build_metrics_body(&state),
    ))
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
            api_keys: ApiKeyStore::default(),
            admin_password_hash: Mutex::new(None),
            dashboard_origin: None,
            query_cache: crate::query::cache::QueryCache::new(0, 0),
            rate_limiter: crate::ingest::ratelimit::RateLimiter::new(0),
            login_attempt_tracker: crate::api::auth::LoginAttemptTracker::new(0, 300),
            events_ingested_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            flush_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            rate_limit_rejections_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            login_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            metrics_token: None,
            query_semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
            secure_cookies: false,
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
    async fn test_metrics_token_auth() {
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
            api_keys: ApiKeyStore::default(),
            admin_password_hash: Mutex::new(None),
            dashboard_origin: None,
            query_cache: crate::query::cache::QueryCache::new(0, 0),
            rate_limiter: crate::ingest::ratelimit::RateLimiter::new(0),
            login_attempt_tracker: crate::api::auth::LoginAttemptTracker::new(0, 300),
            events_ingested_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            flush_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            rate_limit_rejections_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            login_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            metrics_token: Some("secret-token".to_string()),
            query_semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
            secure_cookies: false,
        });
        let _dir = dir;

        // No token -> 401
        let app = build_router(Arc::clone(&state));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Wrong token -> 401
        let app = build_router(Arc::clone(&state));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .header("authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        // Correct token -> 200
        let app = build_router(Arc::clone(&state));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .header("authorization", "Bearer secret-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
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
        assert_eq!(json["buffer_empty"], true);
        assert_eq!(json["auth_configured"], false);
        assert_eq!(json["geoip_loaded"], false);
        assert_eq!(json["filter_bots"], false);
    }

    #[tokio::test]
    async fn test_readiness_check() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
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

        let headers = response.headers();
        assert!(headers.contains_key("x-content-type-options"));
        assert!(headers.contains_key("x-frame-options"));
        assert!(headers.contains_key("referrer-policy"));
        assert!(headers.contains_key("permissions-policy"));
        assert!(headers.contains_key("strict-transport-security"));
        assert!(headers.contains_key("x-request-id"));
    }

    #[tokio::test]
    async fn test_cache_control_on_json_api_response() {
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

        let cache_control = response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            cache_control.contains("no-store"),
            "JSON API responses must have Cache-Control: no-store"
        );
    }

    #[tokio::test]
    async fn test_robots_txt() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/robots.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("User-agent: *"));
        assert!(text.contains("Disallow: /api/"));
    }

    #[tokio::test]
    async fn test_security_txt() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/.well-known/security.txt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("Contact:"));
        assert!(text.contains("Expires:"));
    }

    #[tokio::test]
    async fn test_pixel_track_returns_gif() {
        let (state, _dir) = make_test_state();
        let app = build_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/event?d=example.com&n=pageview&u=https%3A%2F%2Fexample.com%2F")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(content_type, "image/gif");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.len(), 43, "1×1 transparent GIF is always 43 bytes");
        assert_eq!(&body[..6], b"GIF89a");
    }

    #[tokio::test]
    async fn test_hsts_header_present() {
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

        let hsts = response
            .headers()
            .get("strict-transport-security")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            hsts.contains("max-age=31536000"),
            "HSTS must include max-age directive"
        );
        assert!(hsts.contains("includeSubDomains"));
    }

    #[tokio::test]
    async fn test_retry_after_present_on_query_semaphore_429() {
        // Exhaust the semaphore with max_concurrent=0 (unlimited)
        // Use a state with 0 permits to force 429 on the semaphore gate.
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let dir = tempfile::tempdir().unwrap();
        crate::storage::schema::setup_query_view(&conn, dir.path()).unwrap();
        let storage = ParquetStorage::new(dir.path());
        let conn = Arc::new(Mutex::new(conn));
        let buffer = crate::ingest::buffer::EventBuffer::new(1000, conn, storage);
        let state = Arc::new(AppState {
            buffer,
            secret: "test".to_string(),
            allowed_sites: Vec::new(),
            geoip: crate::ingest::geoip::GeoIpReader::open(None),
            filter_bots: false,
            sessions: crate::api::auth::SessionStore::new(3600),
            api_keys: crate::api::auth::ApiKeyStore::default(),
            admin_password_hash: Mutex::new(None),
            dashboard_origin: None,
            query_cache: crate::query::cache::QueryCache::new(0, 0),
            rate_limiter: crate::ingest::ratelimit::RateLimiter::new(0),
            login_attempt_tracker: crate::api::auth::LoginAttemptTracker::new(0, 300),
            events_ingested_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            flush_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            rate_limit_rejections_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            login_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            metrics_token: None,
            query_semaphore: Arc::new(tokio::sync::Semaphore::new(0)), // 0 permits → always 429
            secure_cookies: false,
        });
        let _dir = dir;
        let app = build_router(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/stats/funnel?site_id=test.com&steps=page%3A%2F%2Cpage%3A%2Fabout")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(
            response.headers().contains_key("retry-after"),
            "429 responses must include Retry-After"
        );
    }
}
