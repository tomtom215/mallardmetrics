use crate::api::auth;
use crate::api::stats;
use crate::dashboard;
use crate::ingest::handler::{AppState, ingest_event};
use axum::Router;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::Instrument;

/// Maximum accepted request body, in bytes. A valid event is about 12 KB.
const MAX_BODY_BYTES: usize = 65_536;

/// Upper bounds of the request-latency histogram, in seconds.
///
/// The conventional Prometheus default spread, which is what a stock Grafana
/// dashboard or a `histogram_quantile` recording rule expects to find.
const LATENCY_BUCKETS_SECS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
/// Per-request timeout.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Request counts and latency, recorded for every HTTP request.
///
/// The `/metrics` endpoint previously exported only internal counters —
/// buffered events, cache hits, login failures — and nothing at all about the
/// traffic the process was actually serving. An operator could not answer "is
/// the dashboard slow?", "are we returning 500s?" or "how much load is this?"
/// without a separate reverse-proxy exporter, which the single-binary,
/// no-dependencies deployment is specifically meant to avoid.
///
/// Counts are kept by status class rather than by route: a per-route label set
/// would be unbounded, because `/{*path}` matches anything a crawler asks for.
#[derive(Debug, Default)]
pub struct HttpMetrics {
    /// Responses by status class, indexed `1xx`..`5xx`.
    by_class: [AtomicU64; 5],
    /// Cumulative latency in microseconds — integer, so the counter is exact.
    duration_micros_total: AtomicU64,
    /// Cumulative histogram counts, one per bound in [`LATENCY_BUCKETS_SECS`].
    /// The implicit `+Inf` bucket is the total request count.
    buckets: [AtomicU64; LATENCY_BUCKETS_SECS.len()],
    /// Every observed request, i.e. the `+Inf` bucket.
    observations: AtomicU64,
}

impl HttpMetrics {
    /// Record one completed request.
    pub fn record(&self, status: StatusCode, elapsed: std::time::Duration) {
        let class = (status.as_u16() / 100).clamp(1, 5) as usize - 1;
        self.by_class[class].fetch_add(1, Ordering::Relaxed);

        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.duration_micros_total
            .fetch_add(micros, Ordering::Relaxed);

        let secs = elapsed.as_secs_f64();
        for (bucket, bound) in self.buckets.iter().zip(LATENCY_BUCKETS_SECS) {
            if secs <= bound {
                bucket.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.observations.fetch_add(1, Ordering::Relaxed);
    }
}

/// Time every request and record its outcome.
async fn http_metrics_middleware(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    state
        .http_metrics
        .record(response.status(), started.elapsed());
    response
}

/// Build the Axum router.
pub fn build_router(state: Arc<AppState>) -> Router {
    // Ingestion accepts any origin: the tracker runs on the customer's site.
    let ingestion_cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::POST, Method::GET])
        .allow_headers([header::CONTENT_TYPE]);

    let dashboard_cors = build_dashboard_cors(state.dashboard_origin.as_deref());

    // Reachable without credentials — they are how you obtain them.
    let auth_routes = Router::new()
        .route("/auth/setup", post(auth::auth_setup))
        .route("/auth/login", post(auth::auth_login))
        .route("/auth/logout", post(auth::auth_logout))
        .route("/auth/status", get(auth::auth_status));

    // Admin scope plus CSRF protection.
    let admin_routes = Router::new()
        .route("/keys", post(auth::create_api_key))
        .route("/keys", get(auth::list_api_keys))
        .route("/keys/{key_hash}", delete(auth::revoke_api_key_handler))
        .route("/gdpr/erase", delete(stats::gdpr_erase))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_admin_auth,
        ));

    let stats_routes = Router::new()
        .route("/sites", get(stats::list_sites))
        .route("/stats/main", get(stats::get_main_stats))
        .route("/stats/timeseries", get(stats::get_timeseries))
        .route("/stats/sessions", get(stats::get_sessions))
        .route("/stats/realtime", get(stats::get_realtime))
        .route("/stats/revenue", get(stats::get_revenue))
        .route("/stats/goals", get(stats::get_goals))
        .route("/stats/properties", get(stats::get_property_keys))
        .route("/stats/property-values", get(stats::get_property_values))
        // One parameterised route replaces six near-identical handlers and
        // exposes every dimension the ingest path collects.
        .route("/stats/breakdown/{dimension}", get(stats::get_breakdown))
        .route("/stats/export", get(stats::get_export))
        .route("/stats/funnel", get(stats::get_funnel))
        .route("/stats/retention", get(stats::get_retention))
        .route("/stats/sequences", get(stats::get_sequences))
        .route("/stats/flow", get(stats::get_flow));

    let protected_routes = stats_routes
        .merge(admin_routes)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::require_auth,
        ))
        .layer(dashboard_cors);

    // GET is the pixel / `<img>` tracker, for contexts without JavaScript.
    let ingestion_routes = Router::new()
        .route("/event", post(ingest_event))
        .route("/event", get(pixel_track))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
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
        // The tracker is served from its single source under `tracking/`.
        // `/js/script.js` is an alias for people migrating from Plausible.
        .route("/mallard.js", get(dashboard::serve_tracking_script))
        .route("/js/script.js", get(dashboard::serve_tracking_script))
        .nest("/api", api_routes)
        .route("/", get(dashboard::serve_index))
        .route("/{*path}", get(dashboard::serve_asset))
        .layer(middleware::from_fn(request_id_middleware))
        .layer(axum::middleware::map_response(add_security_headers))
        .layer(CompressionLayer::new())
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            REQUEST_TIMEOUT,
        ))
        .layer(TraceLayer::new_for_http())
        // Added last, so it is the outermost layer and sees the status the
        // client actually receives: a request the timeout layer converts into a
        // 408, or a body the compression layer rewrites, is counted as served.
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            http_metrics_middleware,
        ))
        .with_state(state)
}

/// Add OWASP-recommended security headers and cache directives to every response.
async fn add_security_headers(mut response: Response) -> Response {
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
        HeaderValue::from_static("geolocation=(), microphone=(), camera=(), interest-cohort=()"),
    );
    // Browsers only honour HSTS over HTTPS, so this is inert on plain HTTP.
    headers.insert(
        "strict-transport-security",
        HeaderValue::from_static("max-age=31536000; includeSubDomains; preload"),
    );

    if status == StatusCode::TOO_MANY_REQUESTS && !headers.contains_key("retry-after") {
        headers.insert("retry-after", HeaderValue::from_static("1"));
    }

    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type.contains("text/html") {
        headers.insert(
            "content-security-policy",
            HeaderValue::from_static(
                "default-src 'self'; script-src 'self'; style-src 'self'; \
                 img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; \
                 base-uri 'none'; form-action 'self'",
            ),
        );
    }

    // Analytics responses are per-site and often per-session; a shared cache
    // must never hold them.
    if content_type.contains("application/json") || content_type.contains("text/csv") {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store, no-cache, private"),
        );
    }

    response
}

/// Assign an `X-Request-ID` and put it on the tracing span for the request.
///
/// An upstream proxy's existing value is reused so traces correlate end to end.
async fn request_id_middleware(request: Request<axum::body::Body>, next: Next) -> Response {
    let id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        // A proxy-supplied value is echoed back verbatim, so it is length-capped
        // and restricted to characters that are safe in a header.
        .filter(|v| !v.is_empty() && v.len() <= 128 && v.chars().all(|c| c.is_ascii_graphic()))
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), String::from);

    let span = tracing::info_span!("http_request", request_id = %id);
    let mut response = next.run(request).instrument(span).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// GET /robots.txt — keep the dashboard and API out of search indexes.
async fn robots_txt() -> impl axum::response::IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "User-agent: *\nDisallow: /api/\nDisallow: /health\nDisallow: /metrics\n",
    )
}

/// GET /.well-known/security.txt — RFC 9116 vulnerability reporting policy.
async fn security_txt() -> impl axum::response::IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "# Mallard Metrics — security vulnerability reporting\n\
         # Do NOT open a public GitHub issue for security vulnerabilities.\n\
         # Use the private security advisory form linked below.\n\
         # See also: SECURITY.md in the repository root.\n\
         Contact: https://github.com/tomtom215/mallardmetrics/security/advisories/new\n\
         Expires: 2027-01-01T00:00:00.000Z\n\
         Preferred-Languages: en\n",
    )
}

/// A 1×1 transparent GIF (43 bytes) for the pixel endpoint.
const TRANSPARENT_GIF_1X1: &[u8] = &[
    0x47, 0x49, 0x46, 0x38, 0x39, 0x61, 0x01, 0x00, 0x01, 0x00, 0x80, 0x00, 0x00, 0xff, 0xff, 0xff,
    0x00, 0x00, 0x00, 0x21, 0xf9, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x2c, 0x00, 0x00, 0x00, 0x00,
    0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00, 0x3b,
];

/// GET /api/event — the pixel / `<img>` tracker.
///
/// Takes the same core parameters as the POST endpoint via the query string and
/// always answers with a transparent GIF, so it can be embedded in HTML email
/// and anywhere else JavaScript is unavailable.
async fn pixel_track(
    State(state): State<Arc<AppState>>,
    crate::ingest::handler::PeerAddr(peer): crate::ingest::handler::PeerAddr,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<crate::ingest::handler::PixelParams>,
) -> impl axum::response::IntoResponse {
    crate::ingest::handler::process_pixel_event(&state, &headers, peer, params).await;

    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/gif"),
            // A cached pixel is a lost pageview.
            (header::CACHE_CONTROL, "no-store, no-cache, must-revalidate"),
        ],
        TRANSPARENT_GIF_1X1,
    )
}

/// Build the CORS layer for dashboard and stats routes.
///
/// `dashboard_origin` is validated at startup, so an unparsable value can no
/// longer reach this function — it used to fall back to `*`, which tower-http
/// rejects alongside `Allow-Credentials: true`, turning a config typo into a
/// panic on the first cross-origin request.
fn build_dashboard_cors(dashboard_origin: Option<&str>) -> CorsLayer {
    dashboard_origin
        .and_then(|o| o.parse::<HeaderValue>().ok())
        .map_or_else(
            // No origin configured: allow any, but without credentials, so a
            // third-party page still cannot read a logged-in dashboard's data.
            || {
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods([Method::GET, Method::POST, Method::DELETE])
                    .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
            },
            |origin| {
                CorsLayer::new()
                    .allow_origin(origin)
                    .allow_methods([Method::GET, Method::POST, Method::DELETE])
                    .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION, header::COOKIE])
                    .allow_credentials(true)
            },
        )
}

/// GET /health — liveness. Answers as long as the process is running.
async fn health_check() -> &'static str {
    "ok"
}

/// GET /health/ready — readiness.
///
/// 200 once DuckDB is reachable and `events_all` is queryable, 503 otherwise,
/// so an orchestrator holds traffic until the instance can actually serve.
async fn readiness_check(State(state): State<Arc<AppState>>) -> Response {
    let readers = state.readers.clone();
    let ok = tokio::task::spawn_blocking(move || {
        let conn = readers.acquire();
        let guard = conn.lock();
        guard
            .execute_batch("SELECT 1 FROM events_all LIMIT 0")
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

/// GET /health/detailed — component status and configuration summary.
async fn detailed_health_check(
    State(state): State<Arc<AppState>>,
) -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "buffered_events": state.buffer.len(),
        "buffer_empty": state.buffer.is_empty(),
        "dropped_events": state.buffer.dropped_events.load(Ordering::Relaxed),
        "auth_configured": state.admin_password.is_configured(),
        "active_sessions": state.sessions.len(),
        "geoip_loaded": state.geoip.is_loaded(),
        "behavioral_extension_loaded": state.behavioral_extension_loaded,
        "behavioral_version": state.behavioral_version,
        "read_connections": state.readers.len(),
        "filter_bots": state.filter_bots,
        "trust_proxy_headers": state.trust_proxy_headers,
        "session_window": state.session_window,
        "visitor_salt_rotation_days": state.visitor_salt_rotation_days,
        "cache_entries": state.query_cache.len(),
        "cache_empty": state.query_cache.is_empty(),
    }))
}

/// Emit one metric family: name, help text, type, and value.
fn metric(out: &mut String, name: &str, help: &str, kind: &str, value: u64) {
    use std::fmt::Write;
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    let _ = writeln!(out, "{name} {value}");
}

/// Emit the HTTP request counters and the latency histogram.
fn write_http_metrics(out: &mut String, state: &AppState) {
    use std::fmt::Write;

    let http = &state.http_metrics;

    let _ = writeln!(
        out,
        "# HELP mallard_http_requests_total HTTP responses served, by status class"
    );
    let _ = writeln!(out, "# TYPE mallard_http_requests_total counter");
    for (index, class) in ["1xx", "2xx", "3xx", "4xx", "5xx"].iter().enumerate() {
        let value = http.by_class[index].load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "mallard_http_requests_total{{status=\"{class}\"}} {value}"
        );
    }

    let _ = writeln!(
        out,
        "# HELP mallard_http_request_duration_seconds Time to produce a response"
    );
    let _ = writeln!(
        out,
        "# TYPE mallard_http_request_duration_seconds histogram"
    );
    for (bucket, bound) in http.buckets.iter().zip(LATENCY_BUCKETS_SECS) {
        let value = bucket.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "mallard_http_request_duration_seconds_bucket{{le=\"{bound}\"}} {value}"
        );
    }
    let total = http.observations.load(Ordering::Relaxed);
    let _ = writeln!(
        out,
        "mallard_http_request_duration_seconds_bucket{{le=\"+Inf\"}} {total}"
    );
    // Accumulated in microseconds so the counter stays exact; rendered in
    // seconds because that is the unit the metric name promises.
    #[allow(clippy::cast_precision_loss)]
    let sum_secs = http.duration_micros_total.load(Ordering::Relaxed) as f64 / 1_000_000.0;
    let _ = writeln!(
        out,
        "mallard_http_request_duration_seconds_sum {sum_secs:.6}"
    );
    let _ = writeln!(out, "mallard_http_request_duration_seconds_count {total}");
}

/// Point-in-time values.
fn write_gauges(out: &mut String, state: &AppState) {
    metric(
        out,
        "mallard_buffered_events",
        "Events in the in-memory buffer",
        "gauge",
        state.buffer.len() as u64,
    );
    metric(
        out,
        "mallard_cache_entries",
        "Cached query results",
        "gauge",
        state.query_cache.len() as u64,
    );
    metric(
        out,
        "mallard_active_sessions",
        "Live dashboard sessions",
        "gauge",
        state.sessions.len() as u64,
    );
    metric(
        out,
        "mallard_read_connections",
        "DuckDB connections serving analytics queries",
        "gauge",
        state.readers.len() as u64,
    );
    metric(
        out,
        "mallard_auth_configured",
        "Whether an admin password is set",
        "gauge",
        u64::from(state.admin_password.is_configured()),
    );
    metric(
        out,
        "mallard_geoip_loaded",
        "Whether a GeoIP database is loaded",
        "gauge",
        u64::from(state.geoip.is_loaded()),
    );
    metric(
        out,
        "mallard_behavioral_extension",
        "Whether the DuckDB behavioral extension is loaded",
        "gauge",
        u64::from(state.behavioral_extension_loaded),
    );
    metric(
        out,
        "mallard_filter_bots",
        "Whether bot filtering is enabled",
        "gauge",
        u64::from(state.filter_bots),
    );
}

/// Monotonic totals since process start.
fn write_counters(out: &mut String, state: &AppState) {
    metric(
        out,
        "mallard_events_ingested_total",
        "Events buffered since startup",
        "counter",
        state.events_ingested_total.load(Ordering::Relaxed),
    );
    metric(
        out,
        "mallard_events_dropped_total",
        "Events discarded because the buffer was at capacity",
        "counter",
        state.buffer.dropped_events.load(Ordering::Relaxed),
    );
    metric(
        out,
        "mallard_flush_failures_total",
        "Parquet flush failures since startup",
        "counter",
        state.flush_failures_total.load(Ordering::Relaxed),
    );
    metric(
        out,
        "mallard_rate_limit_rejections_total",
        "Ingest requests rejected by a rate limiter",
        "counter",
        state.rate_limit_rejections_total.load(Ordering::Relaxed),
    );
    metric(
        out,
        "mallard_login_failures_total",
        "Failed login attempts since startup",
        "counter",
        state.login_failures_total.load(Ordering::Relaxed),
    );
    metric(
        out,
        "mallard_cache_hits_total",
        "Query cache hits since startup",
        "counter",
        state.query_cache.hits.load(Ordering::Relaxed),
    );
    metric(
        out,
        "mallard_cache_misses_total",
        "Query cache misses since startup",
        "counter",
        state.query_cache.misses.load(Ordering::Relaxed),
    );
    metric(
        out,
        "mallard_cache_evictions_total",
        "Cache entries evicted to stay within the entry cap",
        "counter",
        state.query_cache.evictions.load(Ordering::Relaxed),
    );
}

/// Render the Prometheus exposition body.
fn build_metrics_body(state: &AppState) -> String {
    let mut out = String::with_capacity(4096);
    write_gauges(&mut out, state);
    write_counters(&mut out, state);
    write_http_metrics(&mut out, state);
    out
}

/// GET /metrics — Prometheus exposition.
///
/// Requires `Authorization: Bearer <token>` when `MALLARD_METRICS_TOKEN` is set.
async fn prometheus_metrics(
    State(state): State<Arc<AppState>>,
    request: Request<axum::body::Body>,
) -> Response {
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
    use crate::test_support::state_builder;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn body_text(response: Response) -> String {
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    // ── Health and observability ─────────────────────────────────────────

    #[tokio::test]
    async fn test_health_check() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state).oneshot(get("/health")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_text(response).await, "ok");
    }

    #[tokio::test]
    async fn test_readiness_check() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(get("/health/ready"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_detailed_health_reports_components() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(get("/health/detailed"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(json["status"], "ok");
        assert!(json.get("version").is_some());
        assert_eq!(json["buffered_events"], 0);
        assert_eq!(json["auth_configured"], false);
        assert_eq!(json["geoip_loaded"], false);
        assert!(json.get("behavioral_version").is_some());
        assert!(json.get("read_connections").is_some());
        assert!(json.get("visitor_salt_rotation_days").is_some());
    }

    #[tokio::test]
    async fn test_prometheus_metrics() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state).oneshot(get("/metrics")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let text = body_text(response).await;
        for expected in [
            "mallard_buffered_events 0",
            "mallard_cache_entries 0",
            "mallard_auth_configured 0",
            "mallard_geoip_loaded 0",
            "mallard_events_dropped_total",
            "mallard_cache_evictions_total",
            "mallard_active_sessions",
        ] {
            assert!(text.contains(expected), "missing metric: {expected}");
        }
    }

    #[tokio::test]
    async fn test_metrics_exposition_is_well_formed() {
        let (state, _dir) = state_builder().build();
        let text = build_metrics_body(&state);
        for line in text.lines() {
            if line.starts_with("# HELP") || line.starts_with("# TYPE") {
                continue;
            }
            let parts: Vec<&str> = line.split(' ').collect();
            assert_eq!(parts.len(), 2, "malformed sample line: {line}");
            // Histogram sums are floats; every other sample is an integer, and
            // `f64` parses both.
            assert!(parts[1].parse::<f64>().is_ok(), "non-numeric value: {line}");
        }
    }

    #[tokio::test]
    async fn test_metrics_token_auth() {
        let (state, _dir) = state_builder()
            .metrics_token(Some("secret-token".to_string()))
            .build();

        for (header_value, expected) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some("Bearer wrong-token"), StatusCode::UNAUTHORIZED),
            (Some("Bearer secret-token"), StatusCode::OK),
        ] {
            let mut request = Request::builder().uri("/metrics");
            if let Some(value) = header_value {
                request = request.header("authorization", value);
            }
            let response = build_router(Arc::clone(&state))
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "for {header_value:?}");
        }
    }

    // ── Ingestion ────────────────────────────────────────────────────────

    async fn post_event(state: Arc<AppState>, payload: serde_json::Value) -> StatusCode {
        build_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/event")
                    .header("content-type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn test_ingest_event() {
        let (state, _dir) = state_builder().build();
        let status = post_event(
            state,
            serde_json::json!({"d": "example.com", "n": "pageview", "u": "https://example.com/"}),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn test_ingest_rejects_missing_fields() {
        let (state, _dir) = state_builder().build();
        assert_eq!(
            post_event(state, serde_json::json!({})).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[tokio::test]
    async fn test_ingest_rejects_empty_fields() {
        let (state, _dir) = state_builder().build();
        let status = post_event(
            state,
            serde_json::json!({"d": "", "n": "pageview", "u": "https://example.com/"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_pixel_track_returns_a_gif() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(get(
                "/api/event?d=example.com&n=pageview&u=https%3A%2F%2Fexample.com%2F",
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("image/gif")
        );
        assert!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .contains("no-store"),
            "a cached pixel is a lost pageview"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.len(), 43);
        assert_eq!(&body[..6], b"GIF89a");
    }

    // ── Routing ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_stats_main_on_an_empty_database() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(get("/api/stats/main?site_id=test.com&period=30d"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_breakdown_route_accepts_every_dimension() {
        let (state, _dir) = state_builder().build();
        for slug in crate::query::breakdowns::Dimension::SLUGS {
            let dim = crate::query::breakdowns::Dimension::from_slug(slug).unwrap();
            let response = build_router(Arc::clone(&state))
                .oneshot(get(&format!(
                    "/api/stats/breakdown/{slug}?site_id=test.com&period=30d"
                )))
                .await
                .unwrap();
            let expected = if dim.requires_behavioral() && !state.behavioral_extension_loaded {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            };
            assert_eq!(response.status(), expected, "dimension {slug}");
        }
    }

    #[tokio::test]
    async fn test_unknown_breakdown_dimension_is_a_bad_request() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(get("/api/stats/breakdown/nonsense?site_id=test.com"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(body_text(response).await.contains("Available"));
    }

    #[tokio::test]
    async fn test_sites_endpoint_lists_configured_sites() {
        let (state, _dir) = state_builder()
            .allowed_sites(vec!["a.com".to_string(), "b.com".to_string()])
            .build();
        let response = build_router(state)
            .oneshot(get("/api/sites"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(json["sites"], serde_json::json!(["a.com", "b.com"]));
    }

    #[tokio::test]
    async fn test_realtime_endpoint_responds() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(get("/api/stats/realtime?site_id=test.com"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_str(&body_text(response).await).unwrap();
        assert_eq!(json["current_visitors"], 0);
    }

    #[tokio::test]
    async fn test_revenue_and_goal_endpoints_respond() {
        let (state, _dir) = state_builder().build();
        for uri in [
            "/api/stats/revenue?site_id=test.com",
            "/api/stats/goals?site_id=test.com",
            "/api/stats/properties?site_id=test.com",
        ] {
            let response = build_router(Arc::clone(&state))
                .oneshot(get(uri))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }
    }

    #[tokio::test]
    async fn test_behavioral_endpoints_report_503_when_unavailable() {
        // Reporting 200 with an empty body made a missing extension look like a
        // site with no data.
        let (state, _dir) = state_builder().build();
        if state.behavioral_extension_loaded {
            return;
        }
        for uri in [
            "/api/stats/funnel?site_id=test.com&steps=page%3A%2F%2Cevent%3Asignup",
            "/api/stats/retention?site_id=test.com&weeks=4",
            "/api/stats/sequences?site_id=test.com&steps=page%3A%2F%2Cevent%3Asignup",
            "/api/stats/flow?site_id=test.com&page=%2F",
        ] {
            let response = build_router(Arc::clone(&state))
                .oneshot(get(uri))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        }
    }

    #[tokio::test]
    async fn test_dashboard_index() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state).oneshot(get("/")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_tracking_script_route() {
        let (state, _dir) = state_builder().build();
        for uri in ["/mallard.js", "/js/script.js"] {
            let response = build_router(Arc::clone(&state))
                .oneshot(get(uri))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }
    }

    #[tokio::test]
    async fn test_not_found() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(get("/nonexistent.file"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_robots_and_security_txt() {
        let (state, _dir) = state_builder().build();
        let robots = build_router(Arc::clone(&state))
            .oneshot(get("/robots.txt"))
            .await
            .unwrap();
        let robots_body = body_text(robots).await;
        assert!(robots_body.contains("Disallow: /api/"));

        let security = build_router(state)
            .oneshot(get("/.well-known/security.txt"))
            .await
            .unwrap();
        let security_body = body_text(security).await;
        assert!(security_body.contains("Contact:"));
        assert!(security_body.contains("Expires:"));
    }

    // ── Headers ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_security_headers_present() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state).oneshot(get("/health")).await.unwrap();
        let headers = response.headers();
        for name in [
            "x-content-type-options",
            "x-frame-options",
            "referrer-policy",
            "permissions-policy",
            "strict-transport-security",
            "x-request-id",
        ] {
            assert!(headers.contains_key(name), "missing header: {name}");
        }
    }

    #[tokio::test]
    async fn test_hsts_is_preload_eligible() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state).oneshot(get("/health")).await.unwrap();
        let hsts = response
            .headers()
            .get("strict-transport-security")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(hsts.contains("max-age=31536000"));
        assert!(hsts.contains("includeSubDomains"));
        assert!(hsts.contains("preload"));
    }

    #[tokio::test]
    async fn test_json_responses_are_not_cached() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(get("/api/stats/main?site_id=test.com&period=30d"))
            .await
            .unwrap();
        let cache_control = response
            .headers()
            .get("cache-control")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(cache_control.contains("no-store"));
        assert!(cache_control.contains("private"));
    }

    #[tokio::test]
    async fn test_html_carries_a_content_security_policy() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state).oneshot(get("/")).await.unwrap();
        let csp = response
            .headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(csp.contains("default-src 'self'"));
        assert!(csp.contains("frame-ancestors 'none'"));
    }

    #[tokio::test]
    async fn test_request_id_is_echoed_when_supplied() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("x-request-id", "upstream-trace-id")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get("x-request-id")
                .and_then(|v| v.to_str().ok()),
            Some("upstream-trace-id")
        );
    }

    #[tokio::test]
    async fn test_hostile_request_id_is_replaced() {
        // The header is echoed back, so an over-long or non-printable value
        // must not be reflected verbatim.
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header("x-request-id", "x".repeat(500))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(id.len(), 36, "expected a generated UUID, got {id:?}");
    }

    #[tokio::test]
    async fn test_cors_headers_on_ingestion() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
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
        assert!(
            response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }

    #[tokio::test]
    async fn test_wildcard_cors_never_allows_credentials() {
        // `Allow-Origin: *` with `Allow-Credentials: true` is rejected by
        // browsers and panics inside tower-http.
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/api/stats/main")
                    .header("origin", "https://elsewhere.example")
                    .header("access-control-request-method", "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let headers = response.headers();
        if headers
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok())
            == Some("*")
        {
            assert!(!headers.contains_key("access-control-allow-credentials"));
        }
    }

    #[tokio::test]
    async fn test_configured_origin_allows_credentials() {
        let (state, _dir) = state_builder()
            .dashboard_origin(Some("https://analytics.example.com".to_string()))
            .build();
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .method("OPTIONS")
                    .uri("/api/stats/main")
                    .header("origin", "https://analytics.example.com")
                    .header("access-control-request-method", "GET")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-credentials")
                .and_then(|v| v.to_str().ok()),
            Some("true")
        );
    }

    #[tokio::test]
    async fn test_retry_after_is_set_on_a_semaphore_rejection() {
        let (state, _dir) = state_builder().query_permits(0).build();
        let response = build_router(state)
            .oneshot(get("/api/stats/main?site_id=test.com"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key("retry-after"));
    }

    #[tokio::test]
    async fn test_concurrency_cap_applies_to_every_analytics_endpoint() {
        // The semaphore previously guarded only four of thirteen endpoints, so
        // it could not bound concurrent database load.
        let (state, _dir) = state_builder().query_permits(0).build();
        for uri in [
            "/api/stats/main?site_id=test.com",
            "/api/stats/timeseries?site_id=test.com",
            "/api/stats/breakdown/pages?site_id=test.com",
            "/api/stats/revenue?site_id=test.com",
            "/api/stats/goals?site_id=test.com",
            "/api/stats/export?site_id=test.com",
        ] {
            let response = build_router(Arc::clone(&state))
                .oneshot(get(uri))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS, "{uri}");
        }
    }

    #[tokio::test]
    async fn test_oversized_body_is_rejected() {
        let (state, _dir) = state_builder().build();
        let response = build_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/event")
                    .header("content-type", "application/json")
                    .body(Body::from("x".repeat(MAX_BODY_BYTES + 1024)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }
}
