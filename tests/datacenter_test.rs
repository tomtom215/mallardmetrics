//! The datacenter traffic filter, end to end through the real router.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mallard_metrics::ingest::asn::DatacenterFilter;
use mallard_metrics::ingest::asn::test_db::{asn_database, v4_bits};
use mallard_metrics::ingest::handler::AppState;
use mallard_metrics::server::build_router;
use mallard_metrics::test_support::state_builder;
use std::sync::Arc;
use tower::ServiceExt;

const AWS: [u8; 4] = [3, 5, 140, 2];
const HOME: [u8; 4] = [81, 2, 69, 142];
const BROWSER: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

fn filtered_state() -> (Arc<AppState>, tempfile::TempDir, tempfile::TempDir) {
    let db = tempfile::tempdir().unwrap();
    let path = db.path().join("asn.mmdb");
    let bytes = asn_database(
        "GeoLite2-ASN",
        4,
        &[(v4_bits(AWS, 32), 16509), (v4_bits(HOME, 32), 3320)],
    );
    std::fs::write(&path, bytes).unwrap();
    let filter = DatacenterFilter::open(&path, &[]).unwrap();
    let (state, dir) = state_builder().datacenter(filter).build();
    (state, dir, db)
}

async fn post(state: &Arc<AppState>, peer: &str, xff: Option<&str>) -> StatusCode {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/event")
        .header("content-type", "application/json")
        .header("user-agent", BROWSER);
    if let Some(xff) = xff {
        builder = builder.header("x-forwarded-for", xff);
    }
    let mut request = builder
        .body(Body::from(
            serde_json::json!({"d": "example.com", "n": "pageview", "u": "https://example.com/"})
                .to_string(),
        ))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        peer.parse::<std::net::SocketAddr>().unwrap(),
    ));
    build_router(Arc::clone(state))
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

async fn metrics(state: &Arc<AppState>) -> String {
    let response = build_router(Arc::clone(state))
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    String::from_utf8_lossy(&response.into_body().collect().await.unwrap().to_bytes()).into_owned()
}

fn counter(text: &str, name: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix(name)?.trim().parse().ok())
        .unwrap_or_else(|| panic!("{name} missing from /metrics"))
}

#[tokio::test]
async fn test_datacenter_event_is_dropped_counted_and_not_stored() {
    let (state, _dir, _db) = filtered_state();
    assert_eq!(
        post(&state, "3.5.140.2:5000", None).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post(&state, "3.5.140.2:5001", None).await,
        StatusCode::ACCEPTED
    );

    let text = metrics(&state).await;
    assert_eq!(counter(&text, "mallard_datacenter_filtered_total"), 2);
    assert_eq!(counter(&text, "mallard_events_ingested_total"), 0);
    assert_eq!(counter(&text, "mallard_buffered_events"), 0);
    assert_eq!(counter(&text, "mallard_datacenter_filter"), 1);
    assert!(
        !text.contains("3.5.140.2"),
        "the address must not reach /metrics"
    );
}

#[tokio::test]
async fn test_normal_traffic_is_unaffected() {
    let (state, _dir, _db) = filtered_state();
    // A listed-database residential ASN, and an address absent from the database.
    assert_eq!(
        post(&state, "81.2.69.142:5000", None).await,
        StatusCode::ACCEPTED
    );
    assert_eq!(
        post(&state, "8.8.8.8:5000", None).await,
        StatusCode::ACCEPTED
    );
    let text = metrics(&state).await;
    assert_eq!(counter(&text, "mallard_events_ingested_total"), 2);
    assert_eq!(counter(&text, "mallard_datacenter_filtered_total"), 0);
}

#[tokio::test]
async fn test_filter_off_by_default() {
    let (state, _dir) = state_builder().build();
    assert_eq!(
        post(&state, "3.5.140.2:5000", None).await,
        StatusCode::ACCEPTED
    );
    let text = metrics(&state).await;
    assert_eq!(counter(&text, "mallard_events_ingested_total"), 1);
    assert_eq!(counter(&text, "mallard_datacenter_filtered_total"), 0);
    assert_eq!(counter(&text, "mallard_datacenter_filter"), 0);
}

#[tokio::test]
async fn test_client_ip_resolution_is_shared_with_the_rate_limiter() {
    // Behind one trusted proxy the client is the right-most X-Forwarded-For
    // entry; the proxy's own address (a datacenter here) must not matter, and a
    // forged left-most entry must not hide a datacenter client.
    let db = tempfile::tempdir().unwrap();
    let path = db.path().join("asn.mmdb");
    std::fs::write(
        &path,
        asn_database("GeoLite2-ASN", 4, &[(v4_bits(AWS, 32), 16509)]),
    )
    .unwrap();
    let (state, _dir) = state_builder()
        .trust_proxy_headers(true)
        .trusted_proxy_hops(1)
        .datacenter(DatacenterFilter::open(&path, &[]).unwrap())
        .build();

    // Proxy peer is the AWS address; real client is 8.8.8.8 -> kept.
    assert_eq!(
        post(&state, "3.5.140.2:1", Some("8.8.8.8")).await,
        StatusCode::ACCEPTED
    );
    // Client is AWS, with a forged residential entry on the left -> dropped.
    assert_eq!(
        post(&state, "10.0.0.1:1", Some("81.2.69.142, 3.5.140.2")).await,
        StatusCode::ACCEPTED
    );
    let text = metrics(&state).await;
    assert_eq!(counter(&text, "mallard_events_ingested_total"), 1);
    assert_eq!(counter(&text, "mallard_datacenter_filtered_total"), 1);
}
