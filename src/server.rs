use crate::api::stats;
use crate::dashboard;
use crate::ingest::handler::{ingest_event, AppState};
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

/// Build the Axum router with all routes.
pub fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let api_routes = Router::new()
        .route("/event", post(ingest_event))
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
        .route("/stats/sessions", get(stats::get_sessions))
        .route("/stats/funnel", get(stats::get_funnel))
        .route("/stats/retention", get(stats::get_retention))
        .route("/stats/sequences", get(stats::get_sequences))
        .route("/stats/flow", get(stats::get_flow));

    Router::new()
        .route("/health", get(health_check))
        .nest("/api", api_routes)
        .route("/", get(dashboard::serve_index))
        .route("/{*path}", get(dashboard::serve_asset))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// GET /health — Health check endpoint.
async fn health_check() -> &'static str {
    "ok"
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let storage = ParquetStorage::new(dir.path());
        let conn = Arc::new(Mutex::new(conn));
        let buffer = EventBuffer::new(1000, conn, storage);
        let state = Arc::new(AppState {
            buffer,
            secret: "test-secret".to_string(),
            allowed_sites: Vec::new(),
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
