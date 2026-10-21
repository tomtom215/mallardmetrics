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

/// Browser User-Agents, for tests that need two distinguishable visitors.
///
/// The visitor ID hashes the User-Agent, so two different ones from the same
/// address are two visitors — which is what a segment test needs.
const CHROME_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36";
const FIREFOX_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";

/// Flush the buffer so events become queryable through `events_all`.
fn flush(state: &Arc<AppState>) {
    state.buffer.flush().expect("flush events");
}

// ── Ingestion ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_full_ingest_pipeline() {
    let (state, _dir) = plain_state();
    // `7d` is the last seven calendar days including today. It used to be
    // eight (today and the seven before it) under a seven-day label.
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
async fn test_pixel_always_answers_with_a_gif() {
    // SI-21: a missing or malformed parameter failed the Query extractor with a
    // text 400 before the handler ran, a broken-image icon in an email client.
    let (state, _dir) = plain_state();
    for uri in [
        "/api/event",
        "/api/event?d=example.com",
        "/api/event?u=https%3A%2F%2Fexample.com%2F",
        "/api/event?d=example.com&u=%2F&w=wide",
        "/api/event?d=bad%20domain&u=%2F",
    ] {
        let response = get(&state, uri).await;
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("image/gif"),
            "{uri}"
        );
    }
    // Only the request that named a site and page (and merely mangled `w`) counts.
    assert_eq!(state.buffer.len(), 1);
    flush(&state);
    let width: Option<String> = {
        let conn = state.buffer.conn().lock();
        conn.query_row("SELECT screen_size FROM events_all LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_eq!(width, None, "a non-numeric width is no width");
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
        7,
        "7d spans seven daily buckets, gaps filled"
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
        let expected = if dim.requires_behavioral() && !state.behavioral.is_loaded() {
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
async fn test_realtime_honours_the_segment() {
    // The endpoint used to accept `filters` and silently ignore it, while the
    // API documentation promised every stats endpoint narrowed by it — so a
    // dashboard filtered to one country still showed everyone as "right now".
    let (state, _dir) = plain_state();
    post_event_with(
        &state,
        pageview("example.com", "https://example.com/"),
        &[("user-agent", CHROME_UA)],
    )
    .await;
    post_event_with(
        &state,
        pageview("example.com", "https://example.com/other"),
        &[("user-agent", FIREFOX_UA)],
    )
    .await;
    flush(&state);

    let all = body_json(get(&state, "/api/stats/realtime?site_id=example.com").await).await;
    assert_eq!(all["current_visitors"], 2);

    let chrome_only = body_json(
        get(
            &state,
            "/api/stats/realtime?site_id=example.com&filters=browsers%3D%3DChrome",
        )
        .await,
    )
    .await;
    assert_eq!(chrome_only["current_visitors"], 1);
    assert_eq!(chrome_only["top_pages"].as_array().unwrap().len(), 1);
    assert_eq!(chrome_only["top_pages"][0]["value"], "/");

    // The per-minute series is part of the same report and must agree with it.
    let series: u64 = chrome_only["per_minute"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(serde_json::Value::as_u64)
        .sum();
    assert_eq!(series, 1, "the per-minute series must be filtered too");
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

// ── Segment filters ──────────────────────────────────────────────────────

/// Two visitors on Chrome from Germany, one on Firefox from the US.
async fn seed_segments(state: &Arc<AppState>) {
    let chrome = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
    let firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0";
    for (ua, path) in [
        (chrome, "/pricing"),
        (chrome, "/docs"),
        (firefox, "/pricing"),
    ] {
        send(
            state,
            Request::builder()
                .method("POST")
                .uri("/api/event")
                .header("content-type", "application/json")
                .header("user-agent", ua)
                .body(Body::from(
                    serde_json::json!({
                        "d": "example.com",
                        "n": "pageview",
                        "u": format!("https://example.com{path}"),
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
    }
    flush(state);
}

#[tokio::test]
async fn test_filters_narrow_every_report() {
    let (state, _dir) = plain_state();
    seed_segments(&state).await;

    let all = body_json(get(&state, "/api/stats/main?site_id=example.com&period=30d").await).await;
    assert_eq!(all["total_pageviews"], 3);

    let chrome = body_json(
        get(
            &state,
            "/api/stats/main?site_id=example.com&period=30d&filters=browsers%3D%3DChrome",
        )
        .await,
    )
    .await;
    assert_eq!(chrome["total_pageviews"], 2, "{chrome}");

    // The same filter must narrow the breakdown too, not just the headline.
    let pages = body_json(
        get(
            &state,
            "/api/stats/breakdown/pages?site_id=example.com&period=30d\
             &filters=browsers%3D%3DFirefox",
        )
        .await,
    )
    .await;
    let rows = pages.as_array().unwrap();
    assert_eq!(rows.len(), 1, "{pages}");
    assert_eq!(rows[0]["value"], "/pricing");
}

#[tokio::test]
async fn test_filtered_and_unfiltered_results_do_not_share_a_cache_entry() {
    // Regression risk: the cache key must carry the segment, or the first
    // request answered would be replayed for every other segment.
    let (state, _dir) = plain_state();
    seed_segments(&state).await;

    let unfiltered =
        body_json(get(&state, "/api/stats/main?site_id=example.com&period=30d").await).await;
    let filtered = body_json(
        get(
            &state,
            "/api/stats/main?site_id=example.com&period=30d&filters=browsers%3D%3DChrome",
        )
        .await,
    )
    .await;
    assert_ne!(
        unfiltered["total_pageviews"], filtered["total_pageviews"],
        "the filtered request was served the unfiltered answer"
    );

    // And asking again returns the same thing rather than the other segment's.
    let again = body_json(
        get(
            &state,
            "/api/stats/main?site_id=example.com&period=30d&filters=browsers%3D%3DChrome",
        )
        .await,
    )
    .await;
    assert_eq!(again["total_pageviews"], filtered["total_pageviews"]);
}

#[tokio::test]
async fn test_malformed_filters_are_rejected() {
    let (state, _dir) = plain_state();
    for bad in [
        "browsers",
        "browsers%3DChrome",
        "nonexistent%3D%3Dx",
        "entry-pages%3D%3D%2F",
        "browsers%3D%3D",
    ] {
        let uri = format!("/api/stats/main?site_id=example.com&period=30d&filters={bad}");
        let response = get(&state, &uri).await;
        assert_eq!(
            response.status(),
            StatusCode::BAD_REQUEST,
            "filters={bad} should be rejected"
        );
    }
}

#[tokio::test]
async fn test_filters_reach_the_timeseries_and_export() {
    let (state, _dir) = plain_state();
    seed_segments(&state).await;

    let series = body_json(
        get(
            &state,
            "/api/stats/timeseries?site_id=example.com&period=30d&filters=browsers%3D%3DChrome",
        )
        .await,
    )
    .await;
    let total: u64 = series
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["pageviews"].as_u64().unwrap())
        .sum();
    assert_eq!(total, 2, "{series}");

    let csv = body_text(
        get(
            &state,
            "/api/stats/export?site_id=example.com&period=30d&kind=raw\
             &filters=browsers%3D%3DFirefox",
        )
        .await,
    )
    .await;
    // Header plus exactly one data row.
    assert_eq!(csv.trim().lines().count(), 2, "{csv}");
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
    if state.behavioral.is_loaded() {
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
    if state.behavioral.is_loaded() {
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
    assert!(!state.admin_password.is_configured());

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
    assert!(state.admin_password.is_configured());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_login_guesses_cannot_exceed_the_attempt_limit() {
    // SI-06 regression: attempts were counted after verification, so a burst of
    // parallel requests all passed the lockout check before any failure was
    // recorded, and every one of them got its own password guess.
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .login_limits(3, 300)
        .build();

    let mut set = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let state = Arc::clone(&state);
        set.spawn(async move {
            post_json(
                &state,
                "/api/auth/login",
                serde_json::json!({"password": "wrong"}),
            )
            .await
            .status()
        });
    }
    let mut verified = 0;
    let mut refused = 0;
    while let Some(status) = set.join_next().await {
        match status.unwrap() {
            StatusCode::UNAUTHORIZED => verified += 1,
            StatusCode::TOO_MANY_REQUESTS => refused += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    assert_eq!(
        verified, 3,
        "exactly the allowed number of guesses is checked"
    );
    assert_eq!(refused, 9);
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

#[tokio::test]
async fn test_login_body_does_not_repeat_the_session_token() {
    // SI-23: the token is a bearer-equivalent credential. It belongs in the
    // HttpOnly cookie only; echoing it in the body defeats HttpOnly.
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .build();
    let response = post_json(
        &state,
        "/api/auth/login",
        serde_json::json!({"password": "correct-horse-battery"}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = session_cookie(&response).expect("a session cookie is set");
    let body = body_text(response).await;
    assert!(
        !body.contains(&cookie),
        "the token must not appear in the body: {body}"
    );
    assert!(!body.contains("token"));
}

async fn post_json_from(
    state: &Arc<AppState>,
    uri: &str,
    payload: serde_json::Value,
    peer: &str,
    origin: Option<&str>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    let mut request = builder.body(Body::from(payload.to_string())).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<std::net::SocketAddr>().unwrap(),
    ));
    send(state, request).await
}

#[tokio::test]
async fn test_login_lockout_cannot_be_dodged_by_rotating_ipv6_addresses() {
    // SI-04 end to end: every guess comes from a different /128 of one /64.
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .login_limits(3, 300)
        .build();
    for i in 1..=3 {
        let response = post_json_from(
            &state,
            "/api/auth/login",
            serde_json::json!({"password": "wrong"}),
            &format!("[2001:db8:7:7::{i}]:4000"),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let fourth = post_json_from(
        &state,
        "/api/auth/login",
        serde_json::json!({"password": "correct-horse-battery"}),
        "[2001:db8:7:7:aaaa::9]:4000",
        None,
    )
    .await;
    assert_eq!(fourth.status(), StatusCode::TOO_MANY_REQUESTS);

    let neighbour = post_json_from(
        &state,
        "/api/auth/login",
        serde_json::json!({"password": "correct-horse-battery"}),
        "[2001:db8:7:8::1]:4000",
        None,
    )
    .await;
    assert_eq!(
        neighbour.status(),
        StatusCode::OK,
        "another /64 is another client"
    );
}

#[tokio::test]
async fn test_auth_endpoints_refuse_a_foreign_origin() {
    // SI-29: login, setup and logout were outside the CSRF check that guards
    // the other state-changing routes.
    let (state, _dir) = state_builder()
        .admin_password("correct-horse-battery")
        .dashboard_origin(Some("https://stats.example.com".to_string()))
        .build();
    let body = serde_json::json!({"password": "correct-horse-battery"});

    let foreign = post_json_from(
        &state,
        "/api/auth/login",
        body.clone(),
        "203.0.113.5:1",
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(foreign.status(), StatusCode::FORBIDDEN);
    assert!(session_cookie(&foreign).is_none());

    let logout = post_json_from(
        &state,
        "/api/auth/logout",
        serde_json::json!({}),
        "203.0.113.5:1",
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(logout.status(), StatusCode::FORBIDDEN);

    let own = post_json_from(
        &state,
        "/api/auth/login",
        body,
        "203.0.113.5:1",
        Some("https://stats.example.com"),
    )
    .await;
    assert_eq!(own.status(), StatusCode::OK);
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

// ── Tracking script ──────────────────────────────────────────────────────

/// Drives `tracking/script.js` against a minimal fake browser (no dependencies,
/// plain Node) and checks what it would send.
const TRACKER_HARNESS: &str = r"// Drives tracking/script.js against a minimal fake browser and checks what it
// would send. Usage: node harness.js <path-to-script.js>
'use strict';
const fs = require('fs');
const vm = require('vm');
const assert = require('assert');

const source = fs.readFileSync(process.argv[2], 'utf8');

/** A fake page: returns { sent, win, doc, fire(target, type, ev) }. */
function page(opts) {
  const attrs = Object.assign({ 'data-domain': 'site.test' }, opts.attrs || {});
  const sent = [];
  const listeners = { window: {}, document: {} };
  const add = (who) => (type, fn) => {
    (listeners[who][type] = listeners[who][type] || []).push(fn);
  };
  const remove = (who) => (type, fn) => {
    listeners[who][type] = (listeners[who][type] || []).filter((f) => f !== fn);
  };
  let url = new URL(opts.url);
  const location = {};
  const syncLocation = () => {
    location.href = url.href;
    location.origin = url.origin;
    location.protocol = url.protocol;
    location.host = url.host;
    location.hostname = url.hostname;
    location.pathname = url.pathname;
    location.search = url.search;
    location.hash = url.hash;
  };
  syncLocation();
  const history = {
    pushState(_s, _t, to) { url = new URL(to, url); syncLocation(); },
    replaceState(_s, _t, to) { url = new URL(to, url); syncLocation(); },
  };
  class FakeBlob { constructor(parts) { this.text = parts.join(''); } }
  const doc = {
    currentScript: { getAttribute: (n) => (n in attrs ? attrs[n] : null), src: 'https://stats.test/mallard.js' },
    baseURI: url.href,
    referrer: opts.referrer || '',
    visibilityState: opts.visibility || 'visible',
    prerendering: !!opts.prerendering,
    addEventListener: add('document'),
    removeEventListener: remove('document'),
    querySelector: () => null,
  };
  const win = {
    location,
    history,
    document: doc,
    navigator: {
      sendBeacon(endpoint, blob) { sent.push(JSON.parse(blob.text)); return true; },
    },
    innerWidth: 1280,
    localStorage: { getItem: () => null },
    addEventListener: add('window'),
    removeEventListener: remove('window'),
  };
  win.window = win;
  const ctx = vm.createContext({
    window: win, document: doc, navigator: win.navigator, URL, Blob: FakeBlob, JSON,
    XMLHttpRequest: function () { throw new Error('XHR not expected'); }, RegExp,
  });
  vm.runInContext(source, ctx);
  const fire = (who, type, ev) => (listeners[who][type] || []).slice().forEach((f) => f(ev || {}));
  return {
    sent, win, doc, fire,
    go(to) { history.pushState(null, '', to); },
    click(link) { fire('document', 'click', { target: { closest: () => link } }); },
  };
}

const tests = [];
const test = (name, fn) => tests.push([name, fn]);

test('SI-10: the external referrer is sent once, not on every SPA route', () => {
  const p = page({ url: 'https://site.test/', referrer: 'https://www.google.com/search?q=x' });
  assert.strictEqual(p.sent.length, 1);
  assert.strictEqual(p.sent[0].r, 'https://www.google.com/search?q=x');
  p.go('/b');
  p.go('/c');
  assert.strictEqual(p.sent.length, 3);
  assert.strictEqual(p.sent[1].r, null, 'second route');
  assert.strictEqual(p.sent[2].r, null, 'third route');
  p.win.mallard('signup');
  assert.strictEqual(p.sent[3].r, null, 'custom events after navigation');
});

test('SI-10: a custom event before any navigation still carries the referrer', () => {
  const p = page({ url: 'https://site.test/', referrer: 'https://www.google.com/' });
  p.win.mallard('signup');
  assert.strictEqual(p.sent[1].r, 'https://www.google.com/');
});

test('SI-11: a hash route is reported as the path', () => {
  const p = page({ url: 'https://site.test/app/#/pricing', attrs: { 'data-hash': 'true' } });
  assert.strictEqual(p.sent[0].u, 'https://site.test/app/pricing');
  p.win.location.hash = '#/docs';
  p.win.history.pushState(null, '', '#/docs');
  p.fire('window', 'hashchange');
  p.fire('window', 'popstate');
  assert.strictEqual(p.sent.length, 2, 'one pageview for one navigation');
  assert.strictEqual(p.sent[1].u, 'https://site.test/app/docs');
});

test('SI-11: the search string survives on a hash route, and a bare fragment is not a route', () => {
  const p = page({ url: 'https://site.test/?utm_source=x#/pricing', attrs: { 'data-hash': 'true' } });
  assert.strictEqual(p.sent[0].u, 'https://site.test/pricing?utm_source=x');
  const q = page({ url: 'https://site.test/page#section', attrs: { 'data-hash': 'true' } });
  assert.strictEqual(q.sent[0].u, 'https://site.test/page#section');
});

test('SI-11: hashchange counts a route change once even with popstate', () => {
  const p = page({ url: 'https://site.test/#/a', attrs: { 'data-hash': 'true' } });
  p.win.history.pushState(null, '', '#/b');
  p.fire('window', 'hashchange');
  p.fire('window', 'hashchange');
  assert.strictEqual(p.sent.length, 2);
});

test('SI-17: a page restored from the back/forward cache is counted', () => {
  const p = page({ url: 'https://site.test/' });
  assert.strictEqual(p.sent.length, 1);
  p.fire('window', 'pageshow', { persisted: false });
  assert.strictEqual(p.sent.length, 1, 'the initial pageshow is not a restore');
  p.fire('window', 'pageshow', { persisted: true });
  assert.strictEqual(p.sent.length, 2);
  assert.strictEqual(p.sent[1].r, null, 'a restore is not a new acquisition');
});

test('SI-18: a document.prerendering page is counted only when activated', () => {
  const p = page({ url: 'https://site.test/', prerendering: true });
  assert.strictEqual(p.sent.length, 0, 'not counted while prerendering');
  p.doc.prerendering = false;
  p.fire('document', 'prerenderingchange');
  p.fire('document', 'visibilitychange');
  assert.strictEqual(p.sent.length, 1, 'counted exactly once on activation');
});

test('SI-18: the legacy prerender visibility state still defers the pageview', () => {
  const p = page({ url: 'https://site.test/', visibility: 'prerender' });
  assert.strictEqual(p.sent.length, 0, 'not counted while prerendering');
  p.doc.visibilityState = 'visible';
  p.fire('document', 'visibilitychange');
  assert.strictEqual(p.sent.length, 1);
});

const link = (href, abs) => ({
  getAttribute: (n) => (n === 'href' ? href : null),
  href: abs === undefined ? new URL(href, 'https://site.test/').href : abs,
});

test('SI-19: outbound and download URLs lose their query and fragment', () => {
  const p = page({ url: 'https://site.test/', attrs: { 'data-outbound': 'true', 'data-downloads': 'true' } });
  p.click(link('https://other.test/path?email=a@b.c&token=s3cret#frag'));
  p.click(link('/files/report.pdf?session=abc#page=2'));
  assert.strictEqual(JSON.parse(p.sent[1].p).url, 'https://other.test/path');
  assert.strictEqual(JSON.parse(p.sent[2].p).url, 'https://site.test/files/report.pdf');
});

test('SI-19: data-link-query opts back in', () => {
  const p = page({
    url: 'https://site.test/',
    attrs: { 'data-outbound': 'true', 'data-link-query': 'true' },
  });
  p.click(link('https://other.test/path?a=1'));
  assert.strictEqual(JSON.parse(p.sent[1].p).url, 'https://other.test/path?a=1');
});

test('SI-19: an SVG link (href is an SVGAnimatedString) is classified by its attribute', () => {
  const p = page({ url: 'https://site.test/', attrs: { 'data-outbound': 'true' } });
  p.click(link('https://other.test/x', { baseVal: 'https://other.test/x', animVal: 'https://other.test/x' }));
  assert.strictEqual(p.sent.length, 2, 'the outbound click was recognised');
  assert.strictEqual(JSON.parse(p.sent[1].p).url, 'https://other.test/x');
});

let failed = 0;
for (const [name, fn] of tests) {
  try { fn(); console.log('ok   ' + name); }
  catch (e) { failed++; console.log('FAIL ' + name + '\n     ' + String(e.message).split('\n')[0]); }
}
process.exit(failed ? 1 : 0);
";

/// Run the tracker harness, skipping where Node is not installed unless the
/// environment says it must be (CI, or `MALLARD_REQUIRE_NODE=1`).
#[test]
fn test_tracking_script_behaviour() {
    let required =
        std::env::var_os("MALLARD_REQUIRE_NODE").is_some() || std::env::var_os("CI").is_some();
    let dir = tempfile::tempdir().unwrap();
    let harness = dir.path().join("tracker_harness.js");
    std::fs::write(&harness, TRACKER_HARNESS).unwrap();

    let output = match std::process::Command::new("node")
        .arg(&harness)
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tracking/script.js"))
        .output()
    {
        Ok(output) => output,
        Err(e) if !required => {
            eprintln!("skipping the tracker harness: node is not available ({e})");
            return;
        }
        Err(e) => panic!("node is required for the tracker harness: {e}"),
    };
    assert!(
        output.status.success(),
        "tracker harness failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

// ── Query execution and metric definitions ───────────────────────────────

#[tokio::test]
async fn test_a_cold_dashboard_load_gets_no_429() {
    // Regression (S-01): a dashboard refresh fires about 31 requests at once
    // (ten summary panels, twenty breakdowns) against a default cap of ten
    // concurrent queries, and every request past the tenth was answered 429 on
    // a cold cache. The cache is disabled here so every request runs a query.
    let (state, _dir) = state_builder().cache_ttl_secs(0).build();
    seed(&state).await;

    let mut uris: Vec<String> = [
        "main?site_id=example.com&period=30d",
        "main?site_id=example.com&period=30d&compare=previous_period",
        "sessions?site_id=example.com&period=30d",
        "timeseries?site_id=example.com&period=30d",
        "goals?site_id=example.com&period=30d",
        "revenue?site_id=example.com&period=30d",
        "properties?site_id=example.com&period=30d",
        "realtime?site_id=example.com",
        "funnel?site_id=example.com&period=30d&steps=page:/,page:/about",
        "retention?site_id=example.com&period=30d&weeks=4",
    ]
    .iter()
    .map(|tail| format!("/api/stats/{tail}"))
    .collect();
    for slug in [
        "pages",
        "referrers",
        "sources",
        "countries",
        "regions",
        "cities",
        "browsers",
        "browser-versions",
        "os",
        "os-versions",
        "devices",
        "screen-sizes",
        "utm-sources",
        "utm-mediums",
        "utm-campaigns",
        "utm-contents",
        "utm-terms",
        "events",
        "entry-pages",
        "exit-pages",
    ] {
        uris.push(format!(
            "/api/stats/breakdown/{slug}?site_id=example.com&period=30d"
        ));
    }
    assert_eq!(uris.len(), 30);

    let mut tasks = Vec::new();
    for uri in uris {
        let state = Arc::clone(&state);
        tasks.push(tokio::spawn(async move {
            let status = get(&state, &uri).await.status();
            (uri, status)
        }));
    }
    for task in tasks {
        let (uri, status) = task.await.unwrap();
        assert_ne!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "{uri} was rejected by the concurrency cap during a dashboard load"
        );
        assert!(
            status == StatusCode::OK || status == StatusCode::SERVICE_UNAVAILABLE,
            "{uri} answered {status}"
        );
    }
}

#[tokio::test]
async fn test_session_analyses_are_refused_when_timestamps_are_rounded() {
    // Regression (S-05): gdpr_mode rounds every timestamp to the hour. Sessions,
    // funnels, sequences and flows then compute from events that can no longer
    // be ordered, and used to return confident, meaningless numbers. They must
    // say so instead; analyses that do not depend on intra-hour order must keep
    // working.
    let (state, _dir) = state_builder().round_timestamps(true).build();
    seed(&state).await;

    for uri in [
        "/api/stats/sessions?site_id=example.com&period=30d",
        "/api/stats/funnel?site_id=example.com&period=30d&steps=page:/,page:/about",
        "/api/stats/sequences?site_id=example.com&period=30d&steps=page:/,page:/about",
        "/api/stats/flow?site_id=example.com&period=30d&page=/",
        "/api/stats/breakdown/entry-pages?site_id=example.com&period=30d",
        "/api/stats/breakdown/exit-pages?site_id=example.com&period=30d",
    ] {
        let response = get(&state, uri).await;
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{uri} must be refused, not computed from rounded timestamps"
        );
        let body = body_json(response).await;
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("rounded"),
            "{uri}: {body}"
        );
    }

    // Headline metrics still answer; only the session-derived fields drop out.
    let main = body_json(get(&state, "/api/stats/main?site_id=example.com&period=30d").await).await;
    assert_eq!(main["total_pageviews"], 3);
    assert_eq!(main["behavioral_available"], false);
    assert!(main["bounce_rate"].is_null(), "{main}");
    assert!(main["total_sessions"].is_null(), "{main}");

    // Unaffected analyses.
    for uri in [
        "/api/stats/breakdown/pages?site_id=example.com&period=30d",
        "/api/stats/timeseries?site_id=example.com&period=30d",
        "/api/stats/goals?site_id=example.com&period=30d",
    ] {
        assert_eq!(get(&state, uri).await.status(), StatusCode::OK, "{uri}");
    }
}

#[tokio::test]
async fn test_a_lone_start_date_is_a_bad_request() {
    // Regression (S-09): it used to be ignored and the 30-day default answered.
    let (state, _dir) = plain_state();
    let response = get(
        &state,
        "/api/stats/main?site_id=example.com&start_date=2024-01-01",
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// ── Streaming raw export ─────────────────────────────────────────────────
//
// The raw export used to build the whole file in memory (several copies of it,
// S-12). It now streams: a blocking thread runs the query and feeds a bounded
// channel that the response body drains. These tests pin the properties that
// make that worth having.

const EXPORT_DAY: &str = "2024-01-15";

/// Insert `count` pageviews for `site` on `day`, one second apart so that
/// `ORDER BY timestamp` has no ties. Straight into the hot table: building this
/// many rows through the ingest endpoint would take far longer than the test.
fn insert_rows(state: &Arc<AppState>, site: &str, day: &str, count: usize) {
    assert!(count <= 86_000, "one row per second of a single day");
    state
        .buffer
        .conn()
        .lock()
        .execute_batch(&format!(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname, browser) \
             SELECT '{site}', 'v' || (i % 100), \
                    TIMESTAMP '{day} 00:00:00' + INTERVAL (i) SECOND, \
                    'pageview', '/p' || (i % 7), \
                    CASE WHEN i % 3 = 0 THEN NULL ELSE 'Chrome' END \
             FROM range({count}) t(i)"
        ))
        .expect("insert rows");
}

fn raw_export_uri(site: &str, format: &str) -> String {
    format!(
        "/api/stats/export?site_id={site}&start_date={EXPORT_DAY}&end_date={EXPORT_DAY}\
         &kind=raw&format={format}"
    )
}

#[tokio::test]
async fn test_streamed_raw_export_is_byte_identical_to_the_buffered_one() {
    use mallard_metrics::query::{QueryScope, export};
    let (state, _dir) = plain_state();
    // Two and a half chunks, so chunk joins are exercised.
    insert_rows(&state, "stream.test", EXPORT_DAY, 25_000);
    let scope = QueryScope::new("stream.test", EXPORT_DAY, "2024-01-16", "30 minutes");
    let rows = {
        let conn = state.buffer.conn();
        let guard = conn.lock();
        export::query_raw_export(&guard, &scope, 100_000).unwrap()
    };
    assert_eq!(rows.len(), 25_000);

    for (format, want, content_type) in [
        (
            "csv",
            export::to_csv(export::RAW_COLUMNS, &rows),
            "text/csv; charset=utf-8",
        ),
        ("json", export::raw_to_json(&rows), "application/json"),
    ] {
        let response = get(&state, &raw_export_uri("stream.test", format)).await;
        assert_eq!(response.status(), StatusCode::OK, "{format}");
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            content_type
        );
        let disposition = response
            .headers()
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            disposition.starts_with("attachment; filename=\"mallard-events-"),
            "{disposition}"
        );
        assert!(
            response.headers().get("content-length").is_none(),
            "a streamed body has no length up front"
        );
        let text = body_text(response).await;
        assert!(
            text == want,
            "{format}: the streamed export differs from the buffered one ({} vs {} bytes)",
            text.len(),
            want.len()
        );
    }
}

#[tokio::test]
async fn test_streamed_raw_export_keeps_its_row_limit() {
    let (state, _dir) = plain_state();
    insert_rows(&state, "stream.test", EXPORT_DAY, 25_000);
    let uri = format!("{}&limit=12345", raw_export_uri("stream.test", "csv"));
    let text = body_text(get(&state, &uri).await).await;
    assert_eq!(text.lines().count(), 12_345 + 1, "header plus the limit");

    let too_many = format!("{}&limit=1000001", raw_export_uri("stream.test", "csv"));
    assert_eq!(
        get(&state, &too_many).await.status(),
        StatusCode::BAD_REQUEST,
        "the cap is unchanged"
    );
}

#[tokio::test]
async fn test_raw_export_with_nothing_to_export_is_still_a_valid_file() {
    let (state, _dir) = plain_state();
    let csv = body_text(get(&state, &raw_export_uri("nobody.test", "csv")).await).await;
    assert_eq!(csv.lines().count(), 1, "just the header: {csv:?}");
    assert!(csv.contains("pathname"));
    let json = body_text(get(&state, &raw_export_uri("nobody.test", "json")).await).await;
    assert_eq!(json, "[]");
}

#[tokio::test]
async fn test_raw_export_streams_with_back_pressure_and_stops_when_the_client_leaves() {
    use std::time::Duration;
    let (state, _dir) = plain_state();
    // Many more chunks (10,000 rows each) than the channel and the socket can
    // hold, so a producer that ignored back-pressure would run to the end.
    insert_rows(&state, "stream.test", EXPORT_DAY, 80_000);

    let response = get(&state, &raw_export_uri("stream.test", "csv")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();

    // Let the producer fill the channel and block on it.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The export still holds the shared tier lock, so an exclusive request (a
    // flush or compaction) cannot get it: the query has not run to completion,
    // which it would have, in milliseconds, if the rows were being collected.
    let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
    let tier = state.tier_lock.clone();
    let writer = std::thread::spawn(move || {
        let _exclusive = tier.write();
        let _ = acquired_tx.send(());
    });
    assert!(
        acquired_rx
            .recv_timeout(Duration::from_millis(400))
            .is_err(),
        "the export finished without anyone reading it: it is not streaming"
    );
    assert!(
        state.query_semaphore.available_permits() < 10,
        "the export should still hold its query slot"
    );

    // Reading makes progress...
    let frame = body.frame().await.expect("a frame").expect("no error");
    assert!(frame.is_data());

    // ...and walking away releases everything promptly.
    drop(body);
    acquired_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the producer kept the tier lock after the client left");
    writer.join().unwrap();
    for _ in 0..100 {
        if state.query_semaphore.available_permits() == 10 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the query slot was not returned after the client left");
}

#[tokio::test]
async fn test_a_client_that_stops_reading_gets_a_truncated_body_not_a_clean_end() {
    use mallard_metrics::api::stats::QueryLimits;
    use std::time::Duration;
    // The deadline bounds DuckDB's own work (a debug build sorting 50,000 rows
    // on a busy machine needs a good fraction of a second) and is also the
    // longest the producer waits for the client to take a chunk, so it is
    // generous and the stall below is longer still.
    let (state, _dir) = state_builder()
        .query_limits(QueryLimits {
            queue_wait: Duration::from_secs(5),
            deadline: Duration::from_secs(2),
        })
        .build();
    // More than the first chunk, the two the channel holds, and the one the
    // producer is blocked sending.
    insert_rows(&state, "stream.test", EXPORT_DAY, 50_000);

    let response = get(&state, &raw_export_uri("stream.test", "csv")).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();

    // Stall well past the deadline, then read what is left.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let mut aborted = false;
    let mut bytes = 0usize;
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else {
            aborted = true;
            break;
        };
        bytes += frame.into_data().map_or(0, |d| d.len());
    }
    assert!(
        aborted,
        "the body ended cleanly after {bytes} bytes: a truncated export would look complete"
    );
}

#[tokio::test]
async fn test_raw_export_answers_busy_before_it_starts_streaming() {
    use mallard_metrics::api::stats::QueryLimits;
    use std::time::Duration;
    let (state, _dir) = state_builder()
        .query_permits(0)
        .query_limits(QueryLimits {
            queue_wait: Duration::from_millis(50),
            deadline: Duration::from_secs(1),
        })
        .build();
    let response = get(&state, &raw_export_uri("stream.test", "csv")).await;
    assert_eq!(
        response.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "an export that cannot get a slot is refused with a status, not a broken body"
    );
}

// ── Site listing ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_sites_are_listed_most_active_first_with_30_day_counts() {
    // The allowlist applies to ingestion too, so it has to name the sites that
    // post events; "configured.test" is listed without ever having any.
    let (state, _dir) = state_builder()
        .allowed_sites(vec![
            "busy.test".to_string(),
            "quiet.test".to_string(),
            "configured.test".to_string(),
        ])
        .build();
    // Today's events, some flushed to Parquet and some still hot.
    for _ in 0..4 {
        post_event(&state, pageview("busy.test", "https://busy.test/")).await;
    }
    post_event(&state, pageview("quiet.test", "https://quiet.test/")).await;
    flush(&state);
    post_event(&state, pageview("busy.test", "https://busy.test/more")).await;
    post_event(&state, pageview("quiet.test", "https://quiet.test/more")).await;
    // A site whose only events are far outside the 30-day window.
    state
        .buffer
        .conn()
        .lock()
        .execute_batch(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname) \
             SELECT 'old.test', 'v' || i, TIMESTAMP '2024-01-15 10:00:00' + INTERVAL (i) MINUTE, \
                    'pageview', '/' FROM range(3) t(i)",
        )
        .unwrap();
    flush(&state);

    let json = body_json(get(&state, "/api/sites").await).await;
    let ids: Vec<&str> = json["sites"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert_eq!(
        ids,
        ["busy.test", "quiet.test", "configured.test", "old.test"],
        "most active first, then the quiet ones alphabetically"
    );
    let counts: Vec<(&str, u64)> = json["details"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["site_id"].as_str().unwrap(),
                d["events_30d"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        counts,
        [
            ("busy.test", 5),
            ("quiet.test", 2),
            ("configured.test", 0),
            ("old.test", 0)
        ]
    );
}

// ── Export memory measurement (not part of the normal run) ───────────────
//
// `cargo test --test ingest_test -- --ignored --nocapture --test-threads=1 \
//     measure_export_memory` prints the peak extra resident memory of exporting
// a million rows the old way (collect every row, then build the CSV) and the
// new way (stream it to a client that discards it as it arrives). Run each in
// its own process (`--exact`) so one cannot inflate the other's baseline.

/// Resident set size in KiB, from `/proc/self/statm` (Linux).
fn rss_kib() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    pages * 4
}

/// Samples this process's resident memory until finished.
struct RssSampler {
    done: Arc<std::sync::atomic::AtomicBool>,
    peak: Arc<std::sync::atomic::AtomicU64>,
    before: u64,
    thread: std::thread::JoinHandle<()>,
}

impl RssSampler {
    fn start() -> Self {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        let done = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(AtomicU64::new(0));
        let before = rss_kib();
        let thread = {
            let (done, peak) = (Arc::clone(&done), Arc::clone(&peak));
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    peak.fetch_max(rss_kib(), Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            })
        };
        Self {
            done,
            peak,
            before,
            thread,
        }
    }

    /// The largest increase over the starting size, in KiB.
    fn finish(self) -> u64 {
        use std::sync::atomic::Ordering;
        self.done.store(true, Ordering::Relaxed);
        self.thread.join().unwrap();
        self.peak
            .load(Ordering::Relaxed)
            .saturating_sub(self.before)
    }
}

fn million_row_state() -> (Arc<AppState>, tempfile::TempDir) {
    let (state, dir) = plain_state();
    for day in 1..=12 {
        insert_rows(&state, "big.test", &format!("2024-01-{day:02}"), 84_000);
    }
    (state, dir)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "memory measurement; see the comment above"]
async fn measure_export_memory_streamed() {
    let (state, _dir) = million_row_state();
    let uri = "/api/stats/export?site_id=big.test&start_date=2024-01-01&end_date=2024-01-12\
               &kind=raw&format=csv&limit=1000000";
    let started = std::time::Instant::now();
    let sampler = RssSampler::start();
    let response = get(&state, uri).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let mut bytes = 0usize;
    while let Some(frame) = body.frame().await {
        bytes += frame.unwrap().into_data().map_or(0, |d| d.len());
    }
    let growth = sampler.finish();
    println!(
        "STREAMED: {bytes} bytes in {:?}; peak RSS growth {} MiB",
        started.elapsed(),
        growth / 1024
    );
}

#[test]
#[ignore = "memory measurement; see the comment above"]
fn measure_export_memory_buffered() {
    use mallard_metrics::query::{QueryScope, export};
    let (state, _dir) = million_row_state();
    let scope = QueryScope::new("big.test", "2024-01-01", "2024-01-13", "30 minutes");
    let started = std::time::Instant::now();
    let conn = state.buffer.conn();
    let sampler = RssSampler::start();
    let rows = {
        let guard = conn.lock();
        export::query_raw_export(&guard, &scope, 1_000_000).unwrap()
    };
    let text = export::to_csv(export::RAW_COLUMNS, &rows);
    let (rows, bytes) = (rows.len(), text.len());
    let growth = sampler.finish();
    println!(
        "BUFFERED: {rows} rows, {bytes} bytes in {:?}; peak RSS growth {} MiB",
        started.elapsed(),
        growth / 1024
    );
}
