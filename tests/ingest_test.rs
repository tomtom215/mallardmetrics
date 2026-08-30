//! End-to-end tests driving the real router.
//!
//! Every test builds its state through `mallard_metrics::test_support`, so a
//! new field on `AppState` needs one edit rather than one per test file.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mallard_metrics::ingest::handler::AppState;
use mallard_metrics::server::build_router;
use mallard_metrics::test_support::{require_behavioral, state_builder};
use std::sync::Arc;
use tower::ServiceExt;

/// A state with no restrictions, plus the temp dir backing its storage.
fn plain_state() -> (Arc<AppState>, tempfile::TempDir) {
    state_builder().build()
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> axum::response::Response {
    build_router(Arc::clone(state))
        .oneshot(request)
        .await
        .unwrap()
}

async fn get(state: &Arc<AppState>, uri: &str) -> axum::response::Response {
    send(
        state,
        Request::builder().uri(uri).body(Body::empty()).unwrap(),
    )
    .await
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Post an event, returning the status.
async fn post_event(state: &Arc<AppState>, payload: serde_json::Value) -> StatusCode {
    post_event_with(state, payload, &[]).await
}

/// Post an event with extra headers.
async fn post_event_with(
    state: &Arc<AppState>,
    payload: serde_json::Value,
    headers: &[(&str, &str)],
) -> StatusCode {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/event")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    send(
        state,
        builder.body(Body::from(payload.to_string())).unwrap(),
    )
    .await
    .status()
}

/// The subset of a stored event the enrichment tests assert on.
struct StoredEvent {
    event_name: String,
    pathname: String,
    utm_source: Option<String>,
    utm_campaign: Option<String>,
    referrer_source: Option<String>,
    revenue_amount: Option<f64>,
    revenue_currency: Option<String>,
}

/// A minimal valid pageview payload.
fn pageview(domain: &str, url: &str) -> serde_json::Value {
    serde_json::json!({ "d": domain, "n": "pageview", "u": url })
}

/// Flush the buffer so events become queryable through `events_all`.
fn flush(state: &Arc<AppState>) {
    state.buffer.flush().expect("flush events");
}

// ── Ingestion ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_full_ingest_pipeline() {
    let (state, _dir) = plain_state();
    assert_eq!(
        post_event(&state, pageview("example.com", "https://example.com/")).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(state.buffer.len(), 1);
    flush(&state);
    assert!(state.buffer.is_empty());
}

#[tokio::test]
async fn test_ingest_with_all_fields() {
    let (state, _dir) = plain_state();
    let payload = serde_json::json!({
        "d": "example.com",
        "n": "purchase",
        "u": "https://example.com/checkout?utm_source=newsletter&utm_campaign=winter%20sale",
        "r": "https://news.ycombinator.com/item?id=1",
        "w": 1920,
        "p": r#"{"plan":"pro"}"#,
        "ra": 49.99,
        "rc": "usd",
    });
    assert_eq!(
        post_event_with(
            &state,
            payload,
            &[(
                "user-agent",
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) Chrome/120.0.0.0 Safari/537.36",
            )],
        )
        .await,
        StatusCode::ACCEPTED
    );
    flush(&state);

    let row = {
        let conn = state.buffer.conn().lock();
        conn.query_row(
            "SELECT event_name, pathname, utm_source, utm_campaign, referrer_source,
                    revenue_amount, revenue_currency
             FROM events_all LIMIT 1",
            [],
            |r| {
                Ok(StoredEvent {
                    event_name: r.get(0)?,
                    pathname: r.get(1)?,
                    utm_source: r.get(2)?,
                    utm_campaign: r.get(3)?,
                    referrer_source: r.get(4)?,
                    revenue_amount: r.get(5)?,
                    revenue_currency: r.get(6)?,
                })
            },
        )
        .unwrap()
    };

    assert_eq!(row.event_name, "purchase");
    assert_eq!(row.pathname, "/checkout");
    assert_eq!(row.utm_source.as_deref(), Some("newsletter"));
    assert_eq!(
        row.utm_campaign.as_deref(),
        Some("winter sale"),
        "UTM values must be percent-decoded"
    );
    assert_eq!(row.referrer_source.as_deref(), Some("Hacker News"));
    assert_eq!(row.revenue_amount, Some(49.99));
    assert_eq!(
        row.revenue_currency.as_deref(),
        Some("USD"),
        "currency must be normalised to uppercase"
    );
}

#[tokio::test]
async fn test_ingest_rejects_invalid_payloads() {
    let (state, _dir) = plain_state();
    let cases = [
        (serde_json::json!({}), StatusCode::UNPROCESSABLE_ENTITY),
        (
            pageview("", "https://example.com/"),
            StatusCode::BAD_REQUEST,
        ),
        (pageview("example.com", ""), StatusCode::BAD_REQUEST),
        (
            pageview("has space.com", "https://x/"),
            StatusCode::BAD_REQUEST,
        ),
        (
            pageview(
                "example.com",
                &format!("https://e.com/{}", "a".repeat(3000)),
            ),
            StatusCode::BAD_REQUEST,
        ),
    ];
    for (payload, expected) in cases {
        assert_eq!(
            post_event(&state, payload.clone()).await,
            expected,
            "{payload}"
        );
    }
}

#[tokio::test]
async fn test_site_allowlist_is_enforced_on_the_payload() {
    // Regression: site_ids was only checked against the Origin header, so a
    // request that omitted Origin could write events for any site.
    let (state, _dir) = state_builder()
        .allowed_sites(vec!["allowed.com".to_string()])
        .build();

    assert_eq!(
        post_event(&state, pageview("allowed.com", "https://allowed.com/")).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_event(&state, pageview("evil.com", "https://evil.com/")).await,
        StatusCode::FORBIDDEN,
        "an unlisted site must be rejected even without an Origin header"
    );
    assert_eq!(state.buffer.len(), 1);
}

#[tokio::test]
async fn test_origin_validation() {
    let (state, _dir) = state_builder()
        .allowed_sites(vec!["example.com".to_string()])
        .build();

    assert_eq!(
        post_event_with(
            &state,
            pageview("example.com", "https://example.com/"),
            &[("origin", "https://example.com")]
        )
        .await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_event_with(
            &state,
            pageview("example.com", "https://example.com/"),
            &[("origin", "https://evil.com")]
        )
        .await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post_event_with(
            &state,
            pageview("example.com", "https://example.com/"),
            &[("origin", "https://example.com.evil.com")]
        )
        .await,
        StatusCode::FORBIDDEN,
        "a prefix match must not pass"
    );
}

#[tokio::test]
async fn test_user_agent_populates_browser_and_os() {
    let (state, _dir) = plain_state();
    let agents = [
        (
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36",
            ("Chrome", "Windows"),
        ),
        (
            "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0",
            ("Firefox", "Linux"),
        ),
    ];

    for (ua, _) in agents {
        post_event_with(
            &state,
            pageview("example.com", "https://example.com/"),
            &[("user-agent", ua)],
        )
        .await;
    }
    flush(&state);

    let conn = state.buffer.conn().lock();
    let mut stmt = conn
        .prepare("SELECT browser, os FROM events_all ORDER BY timestamp")
        .unwrap();
    let rows: Vec<(Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    assert_eq!(rows.len(), 2);
    for (row, (_, expected)) in rows.iter().zip(agents.iter()) {
        assert_eq!(row.0.as_deref(), Some(expected.0));
        assert_eq!(row.1.as_deref(), Some(expected.1));
    }
}

#[tokio::test]
async fn test_bot_traffic_is_filtered_but_accepted() {
    let (state, _dir) = state_builder().filter_bots(true).build();
    assert_eq!(
        post_event_with(
            &state,
            pageview("example.com", "https://example.com/"),
            &[("user-agent", "Mozilla/5.0 (compatible; Googlebot/2.1)")]
        )
        .await,
        StatusCode::ACCEPTED,
        "crawlers get a 202 so they do not retry"
    );
    assert_eq!(state.buffer.len(), 0, "but the event is not stored");
}

#[tokio::test]
async fn test_pixel_endpoint_records_an_event() {
    let (state, _dir) = plain_state();
    let response = get(
        &state,
        "/api/event?d=example.com&n=pageview&u=https%3A%2F%2Fexample.com%2Fabout",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(state.buffer.len(), 1);
    flush(&state);

    let path: String = {
        let conn = state.buffer.conn().lock();
        conn.query_row("SELECT pathname FROM events_all LIMIT 1", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(path, "/about");
}

#[tokio::test]
async fn test_rate_limiting() {
    let (state, _dir) = state_builder().rate_limit_per_site(2).build();
    let payload = pageview("example.com", "https://example.com/");
    assert_eq!(
        post_event(&state, payload.clone()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_event(&state, payload.clone()).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post_event(&state, payload).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        state
            .rate_limit_rejections_total
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[tokio::test]
async fn test_oversized_body_is_rejected() {
    let (state, _dir) = plain_state();
    let response = send(
        &state,
        Request::builder()
            .method("POST")
            .uri("/api/event")
            .header("content-type", "application/json")
            .body(Body::from("x".repeat(100_000)))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_events_ingested_counter() {
    let (state, _dir) = plain_state();
    for _ in 0..3 {
        post_event(&state, pageview("example.com", "https://example.com/")).await;
    }
    assert_eq!(
        state
            .events_ingested_total
            .load(std::sync::atomic::Ordering::Relaxed),
        3
    );

    let text = body_text(get(&state, "/metrics").await).await;
    assert!(text.contains("mallard_events_ingested_total 3"));
}

// ── Storage round trip ───────────────────────────────────────────────────

#[tokio::test]
async fn test_data_survives_a_flush_to_parquet() {
    let (state, _dir) = plain_state();
    for path in ["/", "/about", "/pricing"] {
        post_event(
            &state,
            pageview("example.com", &format!("https://example.com{path}")),
        )
        .await;
    }
    flush(&state);

    // The rows now live only in Parquet; events_all must still find them.
    let (hot, all) = {
        let conn = state.buffer.conn().lock();
        let hot: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        let all: i64 = conn
            .query_row("SELECT COUNT(*) FROM events_all", [], |r| r.get(0))
            .unwrap();
        (hot, all)
    };
    assert_eq!(hot, 0, "flushed rows leave the hot table");
    assert_eq!(all, 3, "and remain visible through the union view");
}

#[tokio::test]
async fn test_sub_second_ordering_survives_the_round_trip() {
    // Timestamps used to be written as whole seconds, which destroyed the
    // ordering that every behavioral function depends on.
    let (state, _dir) = plain_state();
    for path in ["/first", "/second", "/third"] {
        post_event(
            &state,
            pageview("example.com", &format!("https://example.com{path}")),
        )
        .await;
    }
    flush(&state);

    let conn = state.buffer.conn().lock();
    let mut stmt = conn
        .prepare("SELECT pathname FROM events_all ORDER BY timestamp")
        .unwrap();
    let paths: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(paths, vec!["/first", "/second", "/third"]);
}

// ── Stats ────────────────────────────────────────────────────────────────

/// Ingest a small dataset and flush it.
async fn seed(state: &Arc<AppState>) {
    for path in ["/", "/about", "/"] {
        post_event(
            state,
            pageview("example.com", &format!("https://example.com{path}")),
        )
        .await;
    }
    post_event(
        state,
        serde_json::json!({
            "d": "example.com",
            "n": "signup",
            "u": "https://example.com/signup",
            "p": r#"{"plan":"pro"}"#,
            "ra": 25.0,
            "rc": "USD",
        }),
    )
    .await;
    flush(state);
}

#[tokio::test]
async fn test_main_stats_after_ingest() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    let json = body_json(get(&state, "/api/stats/main?site_id=example.com&period=30d").await).await;
    assert_eq!(json["total_pageviews"], 3);
    assert_eq!(json["total_events"], 4);
    assert!(json["unique_visitors"].as_u64().unwrap() >= 1);
    assert!(json.get("views_per_visitor").is_some());
    assert!(json.get("behavioral_available").is_some());
}

#[tokio::test]
async fn test_timeseries_has_no_gaps() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    let json = body_json(
        get(
            &state,
            "/api/stats/timeseries?site_id=example.com&period=7d",
        )
        .await,
    )
    .await;
    let buckets = json.as_array().unwrap();
    assert_eq!(
        buckets.len(),
        8,
        "7d spans eight daily buckets, gaps filled"
    );
    assert!(buckets.iter().all(|b| b.get("date").is_some()));
}

#[tokio::test]
async fn test_breakdowns_across_every_dimension() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    for slug in mallard_metrics::query::breakdowns::Dimension::SLUGS {
        let dim = mallard_metrics::query::breakdowns::Dimension::from_slug(slug).unwrap();
        let response = get(
            &state,
            &format!("/api/stats/breakdown/{slug}?site_id=example.com&period=30d"),
        )
        .await;
        let expected = if dim.requires_behavioral() && !state.behavioral_extension_loaded {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::OK
        };
        assert_eq!(response.status(), expected, "dimension {slug}");
    }
}

#[tokio::test]
async fn test_pages_breakdown_ranks_by_visitors() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    let json = body_json(
        get(
            &state,
            "/api/stats/breakdown/pages?site_id=example.com&period=30d",
        )
        .await,
    )
    .await;
    let rows = json.as_array().unwrap();
    assert!(!rows.is_empty());
    assert_eq!(rows[0]["value"], "/");
    assert_eq!(rows[0]["pageviews"], 2);
}

#[tokio::test]
async fn test_event_name_breakdown_surfaces_custom_events() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    let json = body_json(
        get(
            &state,
            "/api/stats/breakdown/events?site_id=example.com&period=30d",
        )
        .await,
    )
    .await;
    assert!(
        json.as_array()
            .unwrap()
            .iter()
            .any(|r| r["value"].as_str() == Some("signup"))
    );
}

#[tokio::test]
async fn test_goals_endpoint() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    let json =
        body_json(get(&state, "/api/stats/goals?site_id=example.com&period=30d").await).await;
    let goals = json.as_array().unwrap();
    assert_eq!(goals.len(), 1);
    assert_eq!(goals[0]["name"], "signup");
    assert!(goals[0]["conversion_rate"].as_f64().unwrap() > 0.0);
}

#[tokio::test]
async fn test_revenue_endpoint() {
    // Revenue has been accepted at ingest since the first release with no way
    // to read it back.
    let (state, _dir) = plain_state();
    seed(&state).await;

    let json =
        body_json(get(&state, "/api/stats/revenue?site_id=example.com&period=30d").await).await;
    let by_currency = json["by_currency"].as_array().unwrap();
    assert_eq!(by_currency.len(), 1);
    assert_eq!(by_currency[0]["currency"], "USD");
    assert!((by_currency[0]["total"].as_f64().unwrap() - 25.0).abs() < 0.01);
}

#[tokio::test]
async fn test_custom_property_endpoints() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    let keys = body_json(
        get(
            &state,
            "/api/stats/properties?site_id=example.com&period=30d",
        )
        .await,
    )
    .await;
    assert!(
        keys.as_array()
            .unwrap()
            .iter()
            .any(|k| k.as_str() == Some("plan"))
    );

    let values = body_json(
        get(
            &state,
            "/api/stats/property-values?site_id=example.com&period=30d&key=plan",
        )
        .await,
    )
    .await;
    assert_eq!(values[0]["value"], "pro");
}

#[tokio::test]
async fn test_property_key_validation_is_enforced() {
    let (state, _dir) = plain_state();
    let response = get(
        &state,
        "/api/stats/property-values?site_id=example.com&key=%27%3B%20DROP%20TABLE%20events%3B%20--",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_realtime_endpoint() {
    let (state, _dir) = plain_state();
    post_event(&state, pageview("example.com", "https://example.com/")).await;
    flush(&state);

    let json = body_json(get(&state, "/api/stats/realtime?site_id=example.com").await).await;
    assert_eq!(json["current_visitors"], 1);
    assert_eq!(json["window_minutes"], 5);
    assert_eq!(json["top_pages"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_sites_endpoint_discovers_ingested_sites() {
    let (state, _dir) = plain_state();
    post_event(&state, pageview("alpha.com", "https://alpha.com/")).await;
    post_event(&state, pageview("beta.com", "https://beta.com/")).await;
    flush(&state);

    let json = body_json(get(&state, "/api/sites").await).await;
    let sites: Vec<&str> = json["sites"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(sites.contains(&"alpha.com"));
    assert!(sites.contains(&"beta.com"));
}

#[tokio::test]
async fn test_stats_reject_invalid_parameters() {
    let (state, _dir) = plain_state();
    let cases = [
        "/api/stats/main?site_id=bad%20site",
        "/api/stats/main?site_id=",
        "/api/stats/timeseries?site_id=test.com&period=nonsense",
        "/api/stats/main?site_id=test.com&start_date=2000-01-01&end_date=2030-01-01",
        "/api/stats/main?site_id=test.com&start_date=bad&end_date=2024-01-01",
        "/api/stats/breakdown/pages?site_id=test.com&limit=99999",
    ];
    for uri in cases {
        assert_eq!(
            get(&state, uri).await.status(),
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn test_explicit_date_range_includes_the_final_day() {
    // Regression: the inclusive end_date was used as the exclusive bound, so
    // the last day of every custom range was silently dropped.
    let (state, _dir) = plain_state();
    post_event(&state, pageview("example.com", "https://example.com/")).await;
    flush(&state);

    let today = chrono::Utc::now().date_naive().to_string();
    let json = body_json(
        get(
            &state,
            &format!("/api/stats/main?site_id=example.com&start_date={today}&end_date={today}"),
        )
        .await,
    )
    .await;
    assert_eq!(json["total_pageviews"], 1);
}

// ── Export ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_daily_export_csv() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    let response = get(
        &state,
        "/api/stats/export?site_id=example.com&period=7d&kind=daily&format=csv",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/csv; charset=utf-8"
    );
    let text = body_text(response).await;
    assert!(text.starts_with("date,visitors,pageviews,top_page,top_source\n"));
    assert!(text.lines().count() > 1);
}

#[tokio::test]
async fn test_raw_export_excludes_visitor_ids() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    let text = body_text(
        get(
            &state,
            "/api/stats/export?site_id=example.com&period=30d&kind=raw&format=csv",
        )
        .await,
    )
    .await;
    let header = text.lines().next().unwrap();
    assert!(header.contains("pathname"));
    assert!(header.contains("event_name"));
    assert!(
        !header.contains("visitor_id"),
        "a CSV of per-event pseudonyms must never leave the server"
    );
}

#[tokio::test]
async fn test_raw_export_json() {
    let (state, _dir) = plain_state();
    seed(&state).await;

    let json = body_json(
        get(
            &state,
            "/api/stats/export?site_id=example.com&period=30d&kind=raw&format=json",
        )
        .await,
    )
    .await;
    let rows = json.as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows[0].get("pathname").is_some());
    assert!(rows[0].get("visitor_id").is_none());
}

#[tokio::test]
async fn test_export_rejects_unknown_format_and_kind() {
    let (state, _dir) = plain_state();
    for uri in [
        "/api/stats/export?site_id=example.com&format=xml",
        "/api/stats/export?site_id=example.com&kind=everything",
    ] {
        assert_eq!(
            get(&state, uri).await.status(),
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
}

// ── Behavioral analytics ─────────────────────────────────────────────────

/// Seed a funnel: four visitors enter, two reach step 2, one converts.
fn seed_funnel(state: &Arc<AppState>) {
    let conn = state.buffer.conn().lock();
    let rows = [
        ("v1", "2024-01-15 10:00:00", "pageview", "/"),
        ("v1", "2024-01-15 10:05:00", "pageview", "/pricing"),
        ("v1", "2024-01-15 10:10:00", "signup", "/pricing"),
        ("v2", "2024-01-15 11:00:00", "pageview", "/"),
        ("v2", "2024-01-15 11:05:00", "pageview", "/pricing"),
        ("v3", "2024-01-15 12:00:00", "pageview", "/"),
        ("v4", "2024-01-15 13:00:00", "pageview", "/"),
    ];
    for (visitor, ts, name, path) in rows {
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES ('example.com', ?, CAST(? AS TIMESTAMP), ?, ?)",
            duckdb::params![visitor, ts, name, path],
        )
        .unwrap();
    }
}

#[tokio::test]
async fn test_sessions_endpoint_returns_session_metrics() {
    // Every other analytics route had an end-to-end test; this one did not, and
    // an endpoint whose SQL is never executed against a real database is an
    // endpoint that can be shipped broken.
    let (state, _dir) = plain_state();
    if !require_behavioral(&state, "sessions endpoint") {
        return;
    }
    seed_funnel(&state);

    let response = get(
        &state,
        "/api/stats/sessions?site_id=example.com&start_date=2024-01-01&end_date=2024-01-31",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    let json = body_json(response).await;
    // v1 has a three-event session, v2 a two-event one; v3 and v4 have one
    // event apiece, so exactly half the sessions are bounces.
    assert_eq!(json["total_sessions"], 4, "{json}");
    assert!(
        (json["bounce_rate"].as_f64().unwrap() - 0.5).abs() < 1e-9,
        "unexpected bounce rate: {json}"
    );
    // Pages counts pageviews only, so v1's `signup` does not add one:
    // (2 + 2 + 1 + 1) / 4.
    assert!(
        (json["avg_pages_per_session"].as_f64().unwrap() - 1.5).abs() < 1e-9,
        "unexpected pages per session: {json}"
    );
    // v1 spans ten minutes, v2 five, the other two zero: (600 + 300) / 4.
    assert!(
        (json["avg_session_duration_secs"].as_f64().unwrap() - 225.0).abs() < 1e-6,
        "unexpected session duration: {json}"
    );
}

#[tokio::test]
async fn test_sessions_endpoint_reports_a_missing_extension() {
    // Without the extension the route must say so rather than returning zeros,
    // which are indistinguishable from a site where everyone bounced.
    let (state, _dir) = plain_state();
    if state.behavioral_extension_loaded {
        return;
    }
    let response = get(&state, "/api/stats/sessions?site_id=example.com&period=30d").await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn test_funnel_is_cumulative_end_to_end() {
    let (state, _dir) = plain_state();
    if !require_behavioral(&state, "funnel endpoint") {
        return;
    }
    seed_funnel(&state);

    let json = body_json(
        get(
            &state,
            "/api/stats/funnel?site_id=example.com&start_date=2024-01-01&end_date=2024-01-31\
             &steps=page%3A%2F%2Cpage%3A%2Fpricing%2Cevent%3Asignup&window=1%20day",
        )
        .await,
    )
    .await;

    let steps = json.as_array().unwrap();
    assert_eq!(steps.len(), 3);
    assert_eq!(
        steps[0]["visitors"], 4,
        "everyone who entered counts at step 1"
    );
    assert_eq!(steps[1]["visitors"], 2);
    assert_eq!(steps[2]["visitors"], 1);
    assert!((steps[1]["conversion_rate"].as_f64().unwrap() - 0.5).abs() < 1e-9);
    assert_eq!(steps[1]["dropped_off"], 2);
}

#[tokio::test]
async fn test_funnel_rejects_bad_input() {
    let (state, _dir) = plain_state();
    for uri in [
        "/api/stats/funnel?site_id=example.com&steps=invalid",
        "/api/stats/funnel?site_id=example.com&steps=page%3A%2F",
        "/api/stats/funnel?site_id=example.com&steps=page%3A%2F%2Cpage%3A%2Fx&window=999%20days",
        "/api/stats/funnel?site_id=example.com&steps=page%3A%2F%2Cpage%3A%2Fx&modes=drop_tables",
    ] {
        assert_eq!(
            get(&state, uri).await.status(),
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn test_retention_reports_counts_and_the_identity_caveat() {
    let (state, _dir) = plain_state();
    if !require_behavioral(&state, "retention endpoint") {
        return;
    }

    {
        let conn = state.buffer.conn().lock();
        let rows = [
            ("r1", "2024-01-02 10:00:00"),
            ("r1", "2024-01-09 10:00:00"),
            ("r2", "2024-01-03 10:00:00"),
            ("r2", "2024-01-10 10:00:00"),
            ("r3", "2024-01-04 10:00:00"),
            ("r4", "2024-01-05 10:00:00"),
        ];
        for (visitor, ts) in rows {
            conn.execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('example.com', ?, CAST(? AS TIMESTAMP), 'pageview', '/')",
                duckdb::params![visitor, ts],
            )
            .unwrap();
        }
    }

    let json = body_json(
        get(
            &state,
            "/api/stats/retention?site_id=example.com&start_date=2024-01-01\
             &end_date=2024-02-28&weeks=3",
        )
        .await,
    )
    .await;

    let cohorts = json["cohorts"].as_array().unwrap();
    assert_eq!(cohorts.len(), 1);
    assert_eq!(cohorts[0]["cohort_size"], 4);
    assert_eq!(
        cohorts[0]["retained"],
        serde_json::json!([4, 2, 0]),
        "retention must report how many returned, not whether anyone did"
    );

    // The default one-day salt rotation cannot support week-over-week cohorts,
    // and the response has to say so rather than presenting zeros as a finding.
    assert_eq!(json["identity_supports_cohorts"], false);
    assert!(
        json["caveat"]
            .as_str()
            .unwrap()
            .contains("visitor_salt_rotation_days")
    );
}

#[tokio::test]
async fn test_retention_rejects_out_of_range_weeks() {
    let (state, _dir) = plain_state();
    // The behavioral extension accepts 2..=32 conditions; the API used to
    // advertise 1..=52 and reported the resulting binder error as "no data".
    for weeks in ["0", "1", "33", "52"] {
        let response = get(
            &state,
            &format!("/api/stats/retention?site_id=example.com&weeks={weeks}"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "weeks={weeks}");
    }
}

#[tokio::test]
async fn test_retention_caveat_absent_with_a_long_rotation() {
    let (state, _dir) = state_builder().visitor_salt_rotation_days(60).build();
    if !require_behavioral(&state, "retention endpoint") {
        return;
    }
    let json =
        body_json(get(&state, "/api/stats/retention?site_id=example.com&weeks=4").await).await;
    assert_eq!(json["identity_supports_cohorts"], true);
    assert!(json["caveat"].is_null());
}

#[tokio::test]
async fn test_sequence_and_flow_endpoints() {
    let (state, _dir) = plain_state();
    if !require_behavioral(&state, "sequence and flow endpoints") {
        return;
    }
    seed_funnel(&state);

    let range = "start_date=2024-01-01&end_date=2024-01-31";

    let sequences = body_json(
        get(
            &state,
            &format!(
                "/api/stats/sequences?site_id=example.com&{range}\
                 &steps=page%3A%2F%2Cevent%3Asignup"
            ),
        )
        .await,
    )
    .await;
    assert_eq!(sequences["converting_visitors"], 1);
    assert_eq!(sequences["total_visitors"], 4);

    let flow = body_json(
        get(
            &state,
            &format!("/api/stats/flow?site_id=example.com&{range}&page=%2F&direction=forward"),
        )
        .await,
    )
    .await;
    let nodes = flow.as_array().unwrap();
    assert!(!nodes.is_empty());
    assert_eq!(nodes[0]["next_page"], "/pricing");
}

#[tokio::test]
async fn test_flow_rejects_bad_direction_and_page() {
    let (state, _dir) = plain_state();
    for uri in [
        "/api/stats/flow?site_id=example.com&page=",
        "/api/stats/flow?site_id=example.com&page=%2F&direction=sideways",
    ] {
        assert_eq!(
            get(&state, uri).await.status(),
            StatusCode::BAD_REQUEST,
            "{uri}"
        );
    }
}

#[tokio::test]
async fn test_behavioral_endpoints_report_unavailability_clearly() {
    let (state, _dir) = plain_state();
    if state.behavioral_extension_loaded {
        return;
    }
    // A 200 with an empty body was indistinguishable from a site with no data.
    let response = get(
        &state,
        "/api/stats/funnel?site_id=example.com&steps=page%3A%2F%2Cevent%3Asignup",
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let json = body_json(response).await;
    assert!(json["error"].as_str().unwrap().contains("behavioral"));
}

// ── Authentication ───────────────────────────────────────────────────────

/// Extract the `mm_session` cookie value from a response.
fn session_cookie(response: &axum::response::Response) -> Option<String> {
    response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|c| c.split(';').next())
        .and_then(|c| c.strip_prefix("mm_session="))
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

async fn post_json(
    state: &Arc<AppState>,
    uri: &str,
    payload: serde_json::Value,
) -> axum::response::Response {
    send(
        state,
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap(),
    )
    .await
}

#[tokio::test]
async fn test_auth_status_without_a_password() {
    let (state, _dir) = plain_state();
    let json = body_json(get(&state, "/api/auth/status").await).await;
    assert_eq!(json["setup_required"], true);
    assert_eq!(json["authenticated"], true, "open access until setup runs");
}

#[tokio::test]
async fn test_admin_routes_are_refused_before_setup() {
    // Open-access mode makes analytics readable without credentials, which is a
    // deliberate deployment choice. It must not extend to minting API keys or
    // erasing data: a key issued in the window before setup keeps working
    // afterwards, so a few unconfigured minutes would grant permanent access.
    let (state, _dir) = plain_state();
    assert!(state.admin_password_hash.lock().is_none());

    for (method, uri) in [
        ("GET", "/api/keys"),
        ("POST", "/api/keys"),
        ("DELETE", "/api/keys/deadbeef"),
        (
            "DELETE",
            "/api/gdpr/erase?site_id=a.com&start_date=2024-01-01&end_date=2024-01-02",
        ),
    ] {
        let response = send(
            &state,
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"attacker","scope":"admin"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must be refused before setup"
        );
    }
}

#[tokio::test]
async fn test_analytics_stay_readable_before_setup() {
    // The other half of the rule: open access still applies to reads.
    let (state, _dir) = plain_state();
    let response = get(&state, "/api/stats/main?site_id=example.com&period=7d").await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_admin_routes_work_once_an_admin_exists() {
    let (state, _dir) = plain_state();
    let setup = post_json(
        &state,
        "/api/auth/setup",
        serde_json::json!({"password": "a-sufficiently-long-password"}),
    )
    .await;
    assert_eq!(setup.status(), StatusCode::OK);
    let token = session_cookie(&setup).expect("setup returns a session");

    let response = send(
        &state,
        Request::builder()
            .method("GET")
            .uri("/api/keys")
            .header("cookie", format!("mm_session={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_setup_creates_a_password_and_a_session() {
    let (state, _dir) = plain_state();
    let response = post_json(
        &state,
        "/api/auth/setup",
        serde_json::json!({"password": "a-sufficiently-long-password"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(session_cookie(&response).is_some());
    assert!(state.admin_password_hash.lock().is_some());
}

#[tokio::test]
async fn test_setup_enforces_the_minimum_password_length() {
    // Raised from 8: this password guards every visitor record on the instance.
    let (state, _dir) = plain_state();
    let response = post_json(
        &state,
        "/api/auth/setup",
        serde_json::json!({"password": "short12"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        body_json(response).await["error"]
            .as_str()
            .unwrap()
            .contains("12")
    );
}

#[tokio::test]
async fn test_setup_rejects_an_obvious_password() {
    let (state, _dir) = plain_state();
    let response = post_json(
        &state,
        "/api/auth/setup",
        serde_json::json!({"password": "password123"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_setup_cannot_run_twice() {
    let (state, _dir) = state_builder()
        .admin_password("existing-password-here")
        .build();
    let response = post_json(
        &state,
        "/api/auth/setup",
        serde_json::json!({"password": "another-long-password"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_login_and_logout() {
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .build();

    let bad = post_json(
        &state,
        "/api/auth/login",
        serde_json::json!({"password": "wrong-password-here"}),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);

    let good = post_json(
        &state,
        "/api/auth/login",
        serde_json::json!({"password": "correct-horse-battery"}),
    )
    .await;
    assert_eq!(good.status(), StatusCode::OK);
    let token = session_cookie(&good).expect("session cookie");
    assert_eq!(state.sessions.len(), 1);

    let logout = send(
        &state,
        Request::builder()
            .method("POST")
            .uri("/api/auth/logout")
            .header("cookie", format!("mm_session={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(logout.status(), StatusCode::OK);
    assert_eq!(state.sessions.len(), 0);
}

#[tokio::test]
async fn test_stats_require_authentication_once_configured() {
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .build();

    assert_eq!(
        get(&state, "/api/stats/main?site_id=test.com")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );

    let login = post_json(
        &state,
        "/api/auth/login",
        serde_json::json!({"password": "correct-horse-battery"}),
    )
    .await;
    let token = session_cookie(&login).unwrap();

    let authorized = send(
        &state,
        Request::builder()
            .uri("/api/stats/main?site_id=test.com")
            .header("cookie", format!("mm_session={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(authorized.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_ingestion_never_requires_authentication() {
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .build();
    assert_eq!(
        post_event(&state, pageview("example.com", "https://example.com/")).await,
        StatusCode::ACCEPTED
    );
}

#[tokio::test]
async fn test_login_lockout_after_repeated_failures() {
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .login_limits(3, 300)
        .build();

    for _ in 0..3 {
        let response = post_json(
            &state,
            "/api/auth/login",
            serde_json::json!({"password": "wrong"}),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    let locked = post_json(
        &state,
        "/api/auth/login",
        serde_json::json!({"password": "correct-horse-battery"}),
    )
    .await;
    assert_eq!(
        locked.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "even the correct password is refused while locked out"
    );
    assert!(locked.headers().contains_key("retry-after"));
}

#[tokio::test]
async fn test_setup_is_rate_limited_too() {
    // Setup is unauthenticated; without a limit it could be probed freely
    // during the window between first boot and first configuration.
    let (state, _dir) = state_builder().login_limits(2, 300).build();
    for _ in 0..2 {
        post_json(
            &state,
            "/api/auth/setup",
            serde_json::json!({"password": "x"}),
        )
        .await;
    }
    let response = post_json(
        &state,
        "/api/auth/setup",
        serde_json::json!({"password": "a-good-long-password"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
}

// ── API keys ─────────────────────────────────────────────────────────────

async fn admin_session(state: &Arc<AppState>, password: &str) -> String {
    let login = post_json(
        state,
        "/api/auth/login",
        serde_json::json!({"password": password}),
    )
    .await;
    session_cookie(&login).expect("session cookie")
}

#[tokio::test]
async fn test_api_key_lifecycle() {
    let password = "correct-horse-battery";
    let (state, _dir) = state_builder().admin_password(password).build();
    let token = admin_session(&state, password).await;

    let created = send(
        &state,
        Request::builder()
            .method("POST")
            .uri("/api/keys")
            .header("content-type", "application/json")
            .header("cookie", format!("mm_session={token}"))
            .body(Body::from(
                serde_json::json!({"name": "ci", "scope": "ReadOnly"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let body = body_json(created).await;
    let key = body["key"].as_str().unwrap().to_string();
    let key_hash = body["key_hash"].as_str().unwrap().to_string();
    assert!(key.starts_with("mm_"));

    // The key authenticates read endpoints.
    let with_key = send(
        &state,
        Request::builder()
            .uri("/api/stats/main?site_id=test.com")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(with_key.status(), StatusCode::OK);

    // X-API-Key works too.
    let with_header = send(
        &state,
        Request::builder()
            .uri("/api/stats/main?site_id=test.com")
            .header("x-api-key", key.clone())
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(with_header.status(), StatusCode::OK);

    // A read-only key cannot create keys.
    let escalation = send(
        &state,
        Request::builder()
            .method("POST")
            .uri("/api/keys")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::from(
                serde_json::json!({"name": "x", "scope": "Admin"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(escalation.status(), StatusCode::FORBIDDEN);

    // Revoke, and the key stops working.
    let revoked = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri(format!("/api/keys/{key_hash}"))
            .header("cookie", format!("mm_session={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(revoked.status(), StatusCode::OK);

    let after_revoke = send(
        &state,
        Request::builder()
            .uri("/api/stats/main?site_id=test.com")
            .header("authorization", format!("Bearer {key}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(after_revoke.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_key_endpoints_require_authentication() {
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .build();
    assert_eq!(
        get(&state, "/api/keys").await.status(),
        StatusCode::UNAUTHORIZED
    );
}

// ── GDPR erasure ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_gdpr_erase_removes_data_and_clears_the_cache() {
    let password = "correct-horse-battery";
    let (state, _dir) = state_builder()
        .admin_password(password)
        .cache_ttl_secs(600)
        .build();
    let token = admin_session(&state, password).await;

    post_event(&state, pageview("example.com", "https://example.com/")).await;
    flush(&state);

    // Populate the cache so the invalidation is observable.
    let before = send(
        &state,
        Request::builder()
            .uri("/api/stats/main?site_id=example.com&period=30d")
            .header("cookie", format!("mm_session={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body_json(before).await["total_pageviews"], 1);
    assert!(!state.query_cache.is_empty());

    let today = chrono::Utc::now().date_naive().to_string();
    let erased = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri(format!(
                "/api/gdpr/erase?site_id=example.com&start_date={today}&end_date={today}"
            ))
            .header("cookie", format!("mm_session={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(erased.status(), StatusCode::OK);

    // Without cache invalidation the dashboard would keep serving erased data
    // for the whole cache TTL.
    let after = send(
        &state,
        Request::builder()
            .uri("/api/stats/main?site_id=example.com&period=30d")
            .header("cookie", format!("mm_session={token}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(body_json(after).await["total_pageviews"], 0);
}

#[tokio::test]
async fn test_gdpr_erase_requires_admin_authentication() {
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .build();
    let today = chrono::Utc::now().date_naive().to_string();
    let response = send(
        &state,
        Request::builder()
            .method("DELETE")
            .uri(format!(
                "/api/gdpr/erase?site_id=example.com&start_date={today}&end_date={today}"
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

// ── Operational endpoints ────────────────────────────────────────────────

#[tokio::test]
async fn test_health_endpoints() {
    let (state, _dir) = plain_state();
    assert_eq!(get(&state, "/health").await.status(), StatusCode::OK);
    assert_eq!(get(&state, "/health/ready").await.status(), StatusCode::OK);

    let detailed = body_json(get(&state, "/health/detailed").await).await;
    assert_eq!(detailed["status"], "ok");
    assert!(detailed.get("behavioral_version").is_some());
    assert!(detailed.get("read_connections").is_some());
}

#[tokio::test]
async fn test_security_headers_are_present() {
    let (state, _dir) = plain_state();
    let response = get(&state, "/health").await;
    for header in [
        "x-content-type-options",
        "x-frame-options",
        "referrer-policy",
        "permissions-policy",
        "strict-transport-security",
        "x-request-id",
    ] {
        assert!(response.headers().contains_key(header), "missing {header}");
    }
}

#[tokio::test]
async fn test_tracking_script_is_served() {
    let (state, _dir) = plain_state();
    for uri in ["/mallard.js", "/js/script.js"] {
        let response = get(&state, uri).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .and_then(|v| v.to_str().ok()),
            Some("*")
        );
        assert!(body_text(response).await.contains("data-domain"));
    }
}

#[tokio::test]
async fn test_metrics_endpoint_is_open_without_a_token() {
    let (state, _dir) = plain_state();
    let response = get(&state, "/metrics").await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        body_text(response)
            .await
            .contains("mallard_buffered_events")
    );
}

#[tokio::test]
async fn test_metrics_endpoint_honours_its_token() {
    let (state, _dir) = state_builder()
        .metrics_token(Some("scrape-token".to_string()))
        .build();
    assert_eq!(
        get(&state, "/metrics").await.status(),
        StatusCode::UNAUTHORIZED
    );

    let response = send(
        &state,
        Request::builder()
            .uri("/metrics")
            .header("authorization", "Bearer scrape-token")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
}
