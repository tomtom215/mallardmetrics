//! End-to-end tests for share links and saved segments, through the real router.
//!
//! Each security property of `docs/src/share-links.md` has a test here that
//! fails if the property is broken. The ones that matter most are written to
//! look at the *data* a viewer receives, not just a status code.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use mallard_metrics::api::auth::{ApiKeyScope, ApiKeyStore};
use mallard_metrics::api::share::{MAX_ACTIVE_LINKS, ShareState};
use mallard_metrics::ingest::handler::AppState;
use mallard_metrics::server::build_router;
use mallard_metrics::test_support::{require_behavioral, state_builder};
use std::sync::{Arc, Mutex, OnceLock};
use tower::ServiceExt;

const PASSWORD: &str = "correct-horse-battery";
const ADMIN_KEY: &str = "mm_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const READONLY_KEY: &str = "mm_fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const CHROME_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0.0.0 Safari/537.36";
const FIREFOX_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0";
const SITE: &str = "shared.example";
const OTHER_SITE: &str = "other.example";

// ── Log capture ──────────────────────────────────────────────────────────

/// A `Write` that appends to a shared buffer.
#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Sink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Everything the process logs at any level, from every test, for the
/// "the token is never logged" assertions.
fn captured_log() -> &'static Arc<Mutex<Vec<u8>>> {
    static LOG: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOG.get_or_init(|| {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = Sink(Arc::clone(&buffer));
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .try_init();
        buffer
    })
}

fn log_text() -> String {
    String::from_utf8_lossy(&captured_log().lock().unwrap()).into_owned()
}

// ── Harness ──────────────────────────────────────────────────────────────

struct Env {
    state: Arc<AppState>,
    _dir: tempfile::TempDir,
}

fn keys() -> ApiKeyStore {
    let keys = ApiKeyStore::default();
    keys.add_key("admin", ADMIN_KEY, ApiKeyScope::Admin);
    keys.add_key("reader", READONLY_KEY, ApiKeyScope::ReadOnly);
    keys
}

fn env_with(builder: mallard_metrics::test_support::AppStateBuilder) -> Env {
    let _ = captured_log();
    let (state, dir) = builder.admin_password(PASSWORD).api_keys(keys()).build();
    Env { state, _dir: dir }
}

fn env() -> Env {
    env_with(state_builder())
}

async fn send(env: &Env, request: Request<Body>) -> axum::response::Response {
    build_router(Arc::clone(&env.state))
        .oneshot(request)
        .await
        .unwrap()
}

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn text(response: axum::response::Response) -> String {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// An admin API call.
async fn admin(
    env: &Env,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {ADMIN_KEY}"));
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let payload = body.map_or_else(Body::empty, |b| Body::from(b.to_string()));
    send(env, builder.body(payload).unwrap()).await
}

/// A request carrying a share token and nothing else.
async fn viewer(env: &Env, token: &str, uri: &str) -> axum::response::Response {
    send(
        env,
        Request::builder()
            .uri(uri)
            .header("x-share-token", token)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn bare(env: &Env, method: &str, uri: &str) -> axum::response::Response {
    send(
        env,
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn event(env: &Env, ua: &str, site: &str, name: &str, path: &str, revenue: Option<f64>) {
    let mut payload =
        serde_json::json!({"d": site, "n": name, "u": format!("https://{site}{path}")});
    if let Some(amount) = revenue {
        payload["ra"] = serde_json::json!(amount);
        payload["rc"] = serde_json::json!("USD");
    }
    let response = send(
        env,
        Request::builder()
            .method("POST")
            .uri("/api/event")
            .header("content-type", "application/json")
            .header("user-agent", ua)
            .body(Body::from(payload.to_string()))
            .unwrap(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

/// Two browsers on one site, and a second site that must never show through.
async fn seed(env: &Env) {
    event(env, CHROME_UA, SITE, "pageview", "/chrome-page", None).await;
    event(env, CHROME_UA, SITE, "pageview", "/chrome-page", None).await;
    event(env, CHROME_UA, SITE, "chrome_signup", "/chrome-page", None).await;
    event(env, FIREFOX_UA, SITE, "pageview", "/firefox-secret", None).await;
    event(
        env,
        FIREFOX_UA,
        SITE,
        "firefox_signup",
        "/firefox-secret",
        None,
    )
    .await;
    event(
        env,
        FIREFOX_UA,
        SITE,
        "purchase",
        "/firefox-secret",
        Some(99.0),
    )
    .await;
    event(
        env,
        CHROME_UA,
        OTHER_SITE,
        "pageview",
        "/other-site-secret",
        None,
    )
    .await;
    env.state.buffer.flush().expect("flush");
}

/// Create a share link, returning (id, token).
async fn make_link(env: &Env, body: serde_json::Value) -> (String, String) {
    let response = admin(env, "POST", "/api/share-links", Some(body)).await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = json(response).await;
    (
        created["id"].as_str().unwrap().to_string(),
        created["token"].as_str().unwrap().to_string(),
    )
}

async fn plain_link(env: &Env) -> (String, String) {
    make_link(env, serde_json::json!({"label": "plain", "site_id": SITE})).await
}

/// A link restricted to the Chrome segment of `SITE`.
async fn chrome_link(env: &Env) -> (String, String) {
    let segment = admin(
        env,
        "POST",
        "/api/segments",
        Some(serde_json::json!({"site_id": SITE, "name": "Chrome", "filters": "browsers==Chrome"})),
    )
    .await;
    assert_eq!(segment.status(), StatusCode::CREATED);
    let segment_id = json(segment).await["id"].as_str().unwrap().to_string();
    make_link(
        env,
        serde_json::json!({"label": "chrome only", "site_id": SITE, "segment_id": segment_id}),
    )
    .await
}

fn paths(rows: &serde_json::Value) -> Vec<String> {
    rows.as_array()
        .unwrap()
        .iter()
        .map(|r| r["value"].as_str().unwrap().to_string())
        .collect()
}

// ── Authentication: who gets in ──────────────────────────────────────────

#[tokio::test]
async fn test_share_routes_refuse_requests_without_a_valid_token() {
    // The failure limiter has its own test; here it must not mask the status.
    let env =
        env_with(state_builder().share(ShareState::in_memory().with_limits(1000, 1000, 1000)));
    seed(&env).await;
    let (_, token) = plain_link(&env).await;
    let uri = format!("/api/shared/stats/main?site_id={SITE}");

    assert_eq!(
        bare(&env, "GET", &uri).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let mut flipped = token.clone();
    let last = flipped.pop().unwrap();
    flipped.push(if last == '0' { '1' } else { '0' });
    for bad in [
        String::new(),
        "ms_".to_string(),
        "not-a-token".to_string(),
        flipped,
        token.to_uppercase(),
        format!("{token}00"),
        ADMIN_KEY.to_string(),
    ] {
        let response = viewer(&env, &bad, &uri).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{bad:?}");
        let body = text(response).await;
        assert!(
            !body.contains(&bad) || bad.is_empty(),
            "the credential is echoed back: {body}"
        );
    }
    assert_eq!(viewer(&env, &token, &uri).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_a_share_token_opens_nothing_outside_the_share_routes() {
    let env = env();
    seed(&env).await;
    let (id, token) = plain_link(&env).await;
    let bearer = format!("Bearer {token}");

    // Every method and route an admin or reader could use, presented with the
    // share token in every place a credential might be read from.
    let targets = [
        (
            "GET",
            "/api/stats/export?site_id=shared.example&kind=raw&format=csv",
        ),
        (
            "GET",
            "/api/stats/export?site_id=shared.example&kind=daily&format=csv",
        ),
        ("GET", "/api/stats/main?site_id=shared.example"),
        ("GET", "/api/sites"),
        ("GET", "/api/keys"),
        ("POST", "/api/keys"),
        ("DELETE", "/api/keys/abcd"),
        (
            "DELETE",
            "/api/gdpr/erase?site_id=shared.example&start_date=2026-01-01&end_date=2026-01-02",
        ),
        ("POST", "/api/admin/backup"),
        ("GET", "/api/share-links"),
        ("POST", "/api/share-links"),
        ("GET", "/api/segments"),
        ("POST", "/api/segments"),
    ];
    for (method, uri) in targets {
        for (name, value) in [
            ("x-share-token", token.clone()),
            ("authorization", bearer.clone()),
            ("x-api-key", token.clone()),
            ("cookie", format!("mm_session={token}")),
        ] {
            let response = send(
                &env,
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(name, &value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {uri} with a share token in `{name}` must be refused"
            );
        }
    }
    // The link is untouched by all of that.
    let listed = json(admin(&env, "GET", "/api/share-links", None).await).await;
    assert_eq!(listed[0]["id"], id);
    assert_eq!(listed[0]["status"], "active");
}

#[tokio::test]
async fn test_no_export_is_reachable_through_the_share_routes() {
    let env = env();
    seed(&env).await;
    let (_, token) = plain_link(&env).await;
    for uri in [
        "/api/shared/stats/export?site_id=shared.example&kind=raw&format=csv",
        "/api/shared/stats/export?site_id=shared.example&kind=raw&format=json",
        "/api/shared/stats/export?site_id=shared.example&kind=daily&format=csv",
        "/api/shared/export",
        "/api/shared/sites",
        "/api/shared/keys",
        "/api/shared/segments",
        "/api/shared/share-links",
        "/api/shared/admin/backup",
        "/api/shared/gdpr/erase",
        "/api/shared/auth/status",
        "/api/shared/stats/../export",
    ] {
        let response = viewer(&env, &token, uri).await;
        assert!(
            response.status() == StatusCode::NOT_FOUND
                || response.status() == StatusCode::BAD_REQUEST,
            "{uri}: {}",
            response.status()
        );
        let body = text(response).await;
        assert!(
            !body.contains("chrome-page") && !body.contains("timestamp"),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn test_share_routes_only_answer_get() {
    let env = env();
    let (_, token) = plain_link(&env).await;
    for method in ["POST", "PUT", "DELETE", "PATCH"] {
        let response = send(
            &env,
            Request::builder()
                .method(method)
                .uri(format!("/api/shared/stats/main?site_id={SITE}"))
                .header("x-share-token", &token)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::METHOD_NOT_ALLOWED,
            "{method}"
        );
    }
}

// ── Confinement: site and segment ────────────────────────────────────────

#[tokio::test]
async fn test_a_link_cannot_read_another_site() {
    let env = env();
    seed(&env).await;
    let (_, token) = plain_link(&env).await;

    for name in [
        OTHER_SITE,
        "SHARED.EXAMPLE",
        "shared.example.",
        "shared.example%20",
        "",
    ] {
        let response = viewer(
            &env,
            &token,
            &format!("/api/shared/stats/main?site_id={name}"),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::FORBIDDEN,
            "site_id={name:?} must be refused, not rewritten"
        );
        let body = json(response).await;
        assert_eq!(body["code"], "share_site_forbidden");
    }
    // Repeating the parameter cannot smuggle a second site in behind the first.
    for q in [
        format!("site_id={SITE}&site_id={OTHER_SITE}"),
        format!("site_id={OTHER_SITE}&site_id={SITE}"),
    ] {
        let response = viewer(&env, &token, &format!("/api/shared/stats/main?{q}")).await;
        assert!(response.status().is_client_error(), "{q}");
        assert_ne!(response.status(), StatusCode::OK, "{q}");
    }
    // Every whitelisted report enforces it, not just `main`.
    for route in [
        "timeseries",
        "sessions",
        "realtime",
        "revenue",
        "goals",
        "properties",
        "property-values",
        "breakdown/pages",
        "funnel",
        "retention",
        "sequences",
        "flow",
    ] {
        let response = viewer(
            &env,
            &token,
            &format!("/api/shared/stats/{route}?site_id={OTHER_SITE}&period=30d"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{route}");
    }
    // And the only data it does return is its own site's.
    let pages = json(
        viewer(
            &env,
            &token,
            &format!("/api/shared/stats/breakdown/pages?site_id={SITE}&period=30d"),
        )
        .await,
    )
    .await;
    assert!(!paths(&pages).iter().any(|p| p.contains("other-site")));
}

#[tokio::test]
async fn test_an_omitted_site_means_the_links_site() {
    let env = env();
    seed(&env).await;
    let (_, token) = plain_link(&env).await;
    let pages =
        json(viewer(&env, &token, "/api/shared/stats/breakdown/pages?period=30d").await).await;
    let seen = paths(&pages);
    assert!(seen.contains(&"/chrome-page".to_string()), "{seen:?}");
    assert!(!seen.iter().any(|p| p.contains("other-site")), "{seen:?}");
}

#[tokio::test]
async fn test_the_segment_is_applied_to_every_report() {
    let env = env();
    seed(&env).await;
    let (_, token) = chrome_link(&env).await;
    let get = |route: String| {
        let env = &env;
        let token = token.clone();
        async move {
            let response = viewer(env, &token, &format!("/api/shared/stats/{route}")).await;
            let status = response.status();
            (status, json(response).await)
        }
    };

    let (status, main) = get(format!("main?site_id={SITE}&period=30d")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        main["total_pageviews"], 2,
        "only Chrome's pageviews: {main}"
    );
    assert_eq!(
        main["total_events"], 3,
        "Chrome's pageviews and its signup: {main}"
    );

    let (_, pages) = get(format!("breakdown/pages?site_id={SITE}&period=30d")).await;
    assert_eq!(paths(&pages), vec!["/chrome-page"], "{pages}");

    let (_, browsers) = get(format!("breakdown/browsers?site_id={SITE}&period=30d")).await;
    assert_eq!(paths(&browsers), vec!["Chrome"], "{browsers}");

    let (_, series) = get(format!("timeseries?site_id={SITE}&period=30d")).await;
    let total: i64 = series
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["pageviews"].as_i64().unwrap_or(0))
        .sum();
    assert_eq!(total, 2, "{series}");

    let (_, goals) = get(format!("goals?site_id={SITE}&period=30d")).await;
    let goal_names: Vec<String> = goals
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(goal_names.contains(&"chrome_signup".to_string()), "{goals}");
    assert!(
        !goal_names
            .iter()
            .any(|n| n.contains("firefox") || n == "purchase"),
        "{goals}"
    );

    let (_, revenue) = get(format!("revenue?site_id={SITE}&period=30d")).await;
    assert!(
        !revenue.to_string().contains("99"),
        "Firefox's revenue is outside the segment: {revenue}"
    );

    let (_, realtime) = get(format!("realtime?site_id={SITE}")).await;
    assert!(
        !realtime.to_string().contains("firefox-secret"),
        "{realtime}"
    );

    let (_, properties) = get(format!("properties?site_id={SITE}&period=30d")).await;
    assert!(properties.is_array());
}

#[tokio::test]
async fn test_the_segment_applies_to_behavioral_reports_too() {
    let env = env();
    if !require_behavioral(&env.state, "segment-confined behavioral reports") {
        return;
    }
    seed(&env).await;
    let (_, token) = chrome_link(&env).await;
    let funnel = json(
        viewer(
            &env,
            &token,
            &format!(
                "/api/shared/stats/funnel?site_id={SITE}&period=30d\
                 &steps=page%3A%2Fchrome-page,page%3A%2Ffirefox-secret"
            ),
        )
        .await,
    )
    .await;
    // Nobody inside the segment ever reached the second page.
    let reached: Vec<i64> = funnel
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["visitors"].as_i64().unwrap_or(-1))
        .collect();
    assert_eq!(reached.last().copied(), Some(0), "{funnel}");

    let sequences = json(
        viewer(
            &env,
            &token,
            &format!(
                "/api/shared/stats/sequences?site_id={SITE}&period=30d\
                 &steps=page%3A%2Fchrome-page,page%3A%2Ffirefox-secret"
            ),
        )
        .await,
    )
    .await;
    assert_eq!(
        sequences["total_visitors"], 1,
        "only the Chrome visitor exists: {sequences}"
    );
    assert_eq!(sequences["converting_visitors"], 0, "{sequences}");

    let flow = json(
        viewer(
            &env,
            &token,
            &format!("/api/shared/stats/flow?site_id={SITE}&period=30d&page=%2Fchrome-page"),
        )
        .await,
    )
    .await;
    assert!(!flow.to_string().contains("firefox-secret"), "{flow}");
}

#[tokio::test]
async fn test_a_viewer_cannot_remove_or_widen_the_segment() {
    let env = env();
    seed(&env).await;
    let (_, token) = chrome_link(&env).await;
    let pages = |extra: String| {
        let env = &env;
        let token = token.clone();
        async move {
            let response = viewer(
                env,
                &token,
                &format!("/api/shared/stats/breakdown/pages?site_id={SITE}&period=30d{extra}"),
            )
            .await;
            let status = response.status();
            (status, json(response).await)
        }
    };

    // Dropping the parameter, blanking it, or sending only separators.
    for extra in ["", "&filters=", "&filters=%20", "&filters=%3B%3B"] {
        let (status, rows) = pages(extra.to_string()).await;
        assert_eq!(status, StatusCode::OK, "{extra}");
        assert_eq!(paths(&rows), vec!["/chrome-page"], "{extra}: {rows}");
    }
    // Contradicting it narrows to nothing; it never reveals the other browser.
    for filter in [
        "browsers!%3DChrome",
        "browsers%3D%3DFirefox",
        "browsers%3D%3DFirefox%3Bbrowsers!%3DChrome",
    ] {
        let (status, rows) = pages(format!("&filters={filter}")).await;
        assert_eq!(status, StatusCode::OK, "{filter}");
        assert!(rows.as_array().unwrap().is_empty(), "{filter}: {rows}");
    }
    // A second filters parameter is refused, not merged by whichever parser wins.
    let (status, _) = pages("&filters=pages%3D%3D%2Ffoo&filters=".to_string()).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Narrowing further works, and is still inside the segment.
    let (status, rows) = pages("&filters=pages%3D%3D%2Fchrome-page".to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(paths(&rows), vec!["/chrome-page"]);
}

#[tokio::test]
async fn test_an_existing_link_keeps_the_segment_it_was_made_with() {
    let env = env();
    seed(&env).await;
    let segment = json(
        admin(
            &env,
            "POST",
            "/api/segments",
            Some(serde_json::json!({"site_id": SITE, "name": "Chrome", "filters": "browsers==Chrome"})),
        )
        .await,
    )
    .await;
    let segment_id = segment["id"].as_str().unwrap();
    let (_, token) = make_link(
        &env,
        serde_json::json!({"label": "snap", "site_id": SITE, "segment_id": segment_id}),
    )
    .await;
    let uri = format!("/api/shared/stats/breakdown/pages?site_id={SITE}&period=30d");

    // Widen the segment to Firefox, then delete it: the link is unchanged.
    let edited = admin(
        &env,
        "POST",
        &format!("/api/segments/{segment_id}"),
        Some(serde_json::json!({"filters": "browsers==Firefox"})),
    )
    .await;
    assert_eq!(edited.status(), StatusCode::OK);
    assert_eq!(
        paths(&json(viewer(&env, &token, &uri).await).await),
        vec!["/chrome-page"]
    );
    let deleted = admin(&env, "DELETE", &format!("/api/segments/{segment_id}"), None).await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(
        paths(&json(viewer(&env, &token, &uri).await).await),
        vec!["/chrome-page"]
    );

    // The listing shows the snapshot, so an admin sees what the link restricts.
    let listed = json(admin(&env, "GET", "/api/share-links", None).await).await;
    assert_eq!(listed[0]["filters"], "browsers==Chrome");
    assert_eq!(listed[0]["segment"], "Chrome");
}

// ── Lifetime ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_a_revoked_link_is_refused_on_the_very_next_request() {
    let env = env();
    seed(&env).await;
    let (id, token) = plain_link(&env).await;
    let uri = format!("/api/shared/stats/main?site_id={SITE}");
    assert_eq!(viewer(&env, &token, &uri).await.status(), StatusCode::OK);

    let revoked = admin(&env, "DELETE", &format!("/api/share-links/{id}"), None).await;
    assert_eq!(revoked.status(), StatusCode::OK);
    assert_eq!(
        viewer(&env, &token, &uri).await.status(),
        StatusCode::UNAUTHORIZED
    );

    // Revoking again is harmless; an unknown id is a 404.
    assert_eq!(
        admin(&env, "DELETE", &format!("/api/share-links/{id}"), None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        admin(&env, "DELETE", "/api/share-links/nope", None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let listed = json(admin(&env, "GET", "/api/share-links", None).await).await;
    assert_eq!(listed[0]["status"], "revoked");
}

#[tokio::test]
async fn test_an_expired_link_is_refused_on_the_very_next_request() {
    let env = env();
    seed(&env).await;
    let (id, token) = make_link(
        &env,
        serde_json::json!({"label": "short", "site_id": SITE, "expires_in_days": 1}),
    )
    .await;
    let uri = format!("/api/shared/stats/main?site_id={SITE}");
    assert_eq!(viewer(&env, &token, &uri).await.status(), StatusCode::OK);

    let past = chrono::Utc::now().naive_utc() - chrono::Duration::seconds(1);
    env.state.share.links.set_expiry_for_test(&id, Some(past));
    assert_eq!(
        viewer(&env, &token, &uri).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let listed = json(admin(&env, "GET", "/api/share-links", None).await).await;
    assert_eq!(listed[0]["status"], "expired");
    // Expired links do not hold a place against the cap, and an expired link
    // answers exactly like an unknown one.
    let unknown = viewer(&env, &format!("ms_{}", "0".repeat(64)), &uri).await;
    assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_expiry_is_set_from_the_request() {
    let env = env();
    let before = chrono::Utc::now().naive_utc();
    let response = admin(
        &env,
        "POST",
        "/api/share-links",
        Some(serde_json::json!({"label": "week", "site_id": SITE, "expires_in_days": 7})),
    )
    .await;
    let created = json(response).await;
    let expires = chrono::NaiveDateTime::parse_from_str(
        created["expires_at"].as_str().unwrap(),
        "%Y-%m-%dT%H:%M:%SZ",
    )
    .unwrap();
    let delta = (expires - before).num_hours();
    assert!((167..=169).contains(&delta), "{delta}");
    for bad in [0, 731, 100_000] {
        let response = admin(
            &env,
            "POST",
            "/api/share-links",
            Some(serde_json::json!({"label": "x", "site_id": SITE, "expires_in_days": bad})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

// ── Response headers ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_share_responses_are_never_cached_and_send_no_referrer() {
    let env = env();
    seed(&env).await;
    let (id, token) = plain_link(&env).await;
    let ok_uri = format!("/api/shared/stats/main?site_id={SITE}");
    let other_site = format!("/api/shared/stats/main?site_id={OTHER_SITE}");
    let bad_filters = format!("/api/shared/stats/main?site_id={SITE}&filters=bogus");

    let mut responses = vec![
        viewer(&env, &token, &ok_uri).await,
        viewer(
            &env,
            &token,
            &format!("/api/shared/stats/breakdown/pages?site_id={SITE}"),
        )
        .await,
        viewer(&env, &token, "/api/shared/session").await,
        viewer(&env, &token, &other_site).await,
        viewer(&env, &token, &bad_filters).await,
        viewer(&env, "ms_nope", &ok_uri).await,
        bare(&env, "GET", &ok_uri).await,
    ];
    admin(&env, "DELETE", &format!("/api/share-links/{id}"), None).await;
    responses.push(viewer(&env, &token, &ok_uri).await);

    for response in responses {
        let headers = response.headers();
        assert_eq!(
            headers.get("referrer-policy").map(|v| v.to_str().unwrap()),
            Some("no-referrer"),
            "status {}",
            response.status()
        );
        let cache = headers.get("cache-control").unwrap().to_str().unwrap();
        assert!(cache.contains("no-store"), "{cache}");
        assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
    }
}

#[tokio::test]
async fn test_admin_responses_keep_the_ordinary_referrer_policy() {
    // The no-referrer rule is for share responses; it must not have replaced
    // the global header for everything else.
    let env = env();
    let response = bare(&env, "GET", "/health").await;
    assert_eq!(
        response.headers().get("referrer-policy").unwrap(),
        "strict-origin-when-cross-origin"
    );
}

// ── Rate limiting ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_each_link_has_its_own_rate_limit() {
    let share = ShareState::in_memory().with_limits(3, 1000, 1000);
    let env = env_with(state_builder().share(share));
    let (_, busy_token) = plain_link(&env).await;
    let (_, calm_token) = plain_link(&env).await;
    let uri = format!("/api/shared/stats/main?site_id={SITE}");
    let mut statuses = Vec::new();
    for _ in 0..6 {
        statuses.push(viewer(&env, &busy_token, &uri).await.status());
    }
    assert_eq!(statuses[..3], [StatusCode::OK; 3]);
    assert!(
        statuses[3..]
            .iter()
            .all(|s| *s == StatusCode::TOO_MANY_REQUESTS),
        "{statuses:?}"
    );
    let limited = viewer(&env, &busy_token, &uri).await;
    assert!(limited.headers().contains_key("retry-after"));
    assert_eq!(
        limited.headers().get("referrer-policy").unwrap(),
        "no-referrer"
    );
    // Another link is unaffected by the first one's flood.
    assert_eq!(
        viewer(&env, &calm_token, &uri).await.status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_all_links_share_a_global_limit() {
    let share = ShareState::in_memory().with_limits(1000, 4, 1000);
    let env = env_with(state_builder().share(share));
    let (_, a) = plain_link(&env).await;
    let (_, b) = plain_link(&env).await;
    let uri = format!("/api/shared/stats/main?site_id={SITE}");
    let mut ok = 0;
    for i in 0..10 {
        let token = if i % 2 == 0 { &a } else { &b };
        if viewer(&env, token, &uri).await.status() == StatusCode::OK {
            ok += 1;
        }
    }
    // The bucket also refills while the requests run, so the count is a band
    // around the budget, not the budget exactly.
    assert!(
        (4..=6).contains(&ok),
        "the global budget is spent across links: {ok} of 10"
    );
}

#[tokio::test]
async fn test_guessing_tokens_cannot_starve_real_viewers() {
    let share = ShareState::in_memory().with_limits(1000, 1000, 3);
    let env = env_with(state_builder().share(share));
    let (_, token) = plain_link(&env).await;
    let uri = format!("/api/shared/stats/main?site_id={SITE}");
    let mut refused = Vec::new();
    for i in 0..8 {
        let guess = format!("ms_{i:064x}");
        refused.push(viewer(&env, &guess, &uri).await.status());
    }
    assert_eq!(refused[0], StatusCode::UNAUTHORIZED);
    assert_eq!(refused[7], StatusCode::TOO_MANY_REQUESTS, "{refused:?}");
    // The valid link, from the same address, is still served.
    for _ in 0..5 {
        assert_eq!(viewer(&env, &token, &uri).await.status(), StatusCode::OK);
    }
}

// ── Admin endpoints ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_share_admin_endpoints_need_an_admin() {
    let env = env();
    let body = serde_json::json!({"label": "x", "site_id": SITE});
    for (method, uri) in [
        ("GET", "/api/share-links"),
        ("POST", "/api/share-links"),
        ("DELETE", "/api/share-links/abc"),
        ("POST", "/api/segments"),
        ("POST", "/api/segments/abc"),
        ("DELETE", "/api/segments/abc"),
    ] {
        let anonymous = send(
            &env,
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(
            anonymous.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri}"
        );

        let reader = send(
            &env,
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {READONLY_KEY}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(reader.status(), StatusCode::FORBIDDEN, "{method} {uri}");
    }
}

#[tokio::test]
async fn test_share_admin_endpoints_are_closed_before_an_admin_exists() {
    let (state, dir) = state_builder().build();
    let env = Env { state, _dir: dir };
    let response = bare(&env, "GET", "/api/share-links").await;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_share_admin_endpoints_apply_the_csrf_check() {
    let env = env_with(state_builder().dashboard_origin(Some("https://stats.example.com".into())));
    let login = send(
        &env,
        Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"password": PASSWORD}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    let cookie = login
        .headers()
        .get("set-cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let attempt = |origin: &'static str| {
        let env = &env;
        let cookie = cookie.clone();
        async move {
            send(
                env,
                Request::builder()
                    .method("POST")
                    .uri("/api/share-links")
                    .header("content-type", "application/json")
                    .header("cookie", cookie)
                    .header("origin", origin)
                    .body(Body::from(
                        serde_json::json!({"label": "x", "site_id": SITE}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .status()
        }
    };
    assert_eq!(attempt("https://evil.example").await, StatusCode::FORBIDDEN);
    assert_eq!(
        attempt("https://stats.example.com").await,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn test_the_token_is_shown_once_and_never_listed() {
    let env = env();
    let response = admin(
        &env,
        "POST",
        "/api/share-links",
        Some(serde_json::json!({"label": "board", "site_id": SITE})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::CREATED);
    let created = json(response).await;
    let token = created["token"].as_str().unwrap().to_string();
    assert!(token.starts_with("ms_"));
    assert_eq!(token.len(), 3 + 64, "256 bits of hex after the prefix");

    let listed = admin(&env, "GET", "/api/share-links", None).await;
    let body = text(listed).await;
    assert!(!body.contains(&token));
    assert!(!body.contains(&token[3..]));
    assert!(
        !body.contains("token"),
        "no token or token_hash field: {body}"
    );
    let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
    let row = &listed[0];
    for field in [
        "id",
        "label",
        "site_id",
        "segment",
        "created_at",
        "expires_at",
        "last_used_at",
        "status",
    ] {
        assert!(row.get(field).is_some(), "{field} missing from {row}");
    }
    assert_eq!(row["label"], "board");
    assert_eq!(row["site_id"], SITE);
}

#[tokio::test]
async fn test_create_validates_its_input() {
    let env = env();
    for body in [
        serde_json::json!({"label": "", "site_id": SITE}),
        serde_json::json!({"label": "x".repeat(129), "site_id": SITE}),
        serde_json::json!({"label": "bad\nlabel", "site_id": SITE}),
        serde_json::json!({"label": "x", "site_id": "../etc"}),
        serde_json::json!({"label": "x", "site_id": ""}),
        serde_json::json!({"label": "x"}),
        serde_json::json!({"site_id": SITE}),
    ] {
        let response = admin(&env, "POST", "/api/share-links", Some(body.clone())).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{body}");
    }
    let missing = admin(
        &env,
        "POST",
        "/api/share-links",
        Some(serde_json::json!({"label": "x", "site_id": SITE, "segment_id": "nope"})),
    )
    .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);

    // A segment of another site cannot be attached.
    let segment = json(
        admin(
            &env,
            "POST",
            "/api/segments",
            Some(serde_json::json!({"site_id": OTHER_SITE, "name": "o", "filters": "browsers==Chrome"})),
        )
        .await,
    )
    .await;
    let cross = admin(
        &env,
        "POST",
        "/api/share-links",
        Some(serde_json::json!({"label": "x", "site_id": SITE, "segment_id": segment["id"]})),
    )
    .await;
    assert_eq!(cross.status(), StatusCode::BAD_REQUEST);
    assert!(
        json(admin(&env, "GET", "/api/share-links", None).await)
            .await
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn test_the_number_of_links_is_capped() {
    let env = env();
    let mut first = None;
    for i in 0..MAX_ACTIVE_LINKS {
        let (id, _) = make_link(
            &env,
            serde_json::json!({"label": format!("l{i}"), "site_id": SITE}),
        )
        .await;
        first.get_or_insert(id);
    }
    let over = admin(
        &env,
        "POST",
        "/api/share-links",
        Some(serde_json::json!({"label": "one too many", "site_id": SITE})),
    )
    .await;
    assert_eq!(over.status(), StatusCode::CONFLICT);
    admin(
        &env,
        "DELETE",
        &format!("/api/share-links/{}", first.unwrap()),
        None,
    )
    .await;
    let again = admin(
        &env,
        "POST",
        "/api/share-links",
        Some(serde_json::json!({"label": "fits", "site_id": SITE})),
    )
    .await;
    assert_eq!(again.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn test_the_session_endpoint_tells_the_viewer_only_what_it_needs() {
    let env = env();
    let (_, token) = chrome_link(&env).await;
    let session = json(viewer(&env, &token, "/api/shared/session").await).await;
    assert_eq!(session["site_id"], SITE);
    assert_eq!(session["segment"], "Chrome");
    assert!(session["expires_at"].is_null());
    let rendered = session.to_string();
    assert!(
        !rendered.contains("chrome only"),
        "the admin's label is private"
    );
    assert!(
        !rendered.contains("browsers"),
        "the segment's filters are private"
    );
}

#[tokio::test]
async fn test_last_use_shows_in_the_listing() {
    let env = env();
    seed(&env).await;
    let (_, token) = plain_link(&env).await;
    let before = json(admin(&env, "GET", "/api/share-links", None).await).await;
    assert!(before[0]["last_used_at"].is_null());
    viewer(
        &env,
        &token,
        &format!("/api/shared/stats/main?site_id={SITE}"),
    )
    .await;
    let after = json(admin(&env, "GET", "/api/share-links", None).await).await;
    assert!(after[0]["last_used_at"].is_string(), "{after}");
}

// ── Persistence ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_only_hashes_reach_the_disk_and_they_survive_a_restart() {
    let data = tempfile::tempdir().unwrap();
    let env = env_with(state_builder().share(ShareState::load(data.path())));
    seed(&env).await;
    let (id, token) = chrome_link(&env).await;

    let stored = std::fs::read_to_string(data.path().join("share_links.json")).unwrap();
    assert!(
        !stored.contains(&token) && !stored.contains(&token[3..]),
        "{stored}"
    );
    assert!(stored.contains(&id));
    let segments = std::fs::read_to_string(data.path().join("segments.json")).unwrap();
    assert!(segments.contains("browsers==Chrome"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in ["share_links.json", "segments.json"] {
            let mode = std::fs::metadata(data.path().join(file))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{file}");
        }
    }

    // A new process (a new state over the same directory) honours the link,
    // with its segment, without being told the token again.
    let restarted = env_with(state_builder().share(ShareState::load(data.path())));
    let pages = json(
        viewer(
            &restarted,
            &token,
            &format!("/api/shared/stats/breakdown/pages?site_id={SITE}&period=30d"),
        )
        .await,
    )
    .await;
    assert_eq!(
        pages.as_array().unwrap().len(),
        0,
        "no events in the new database"
    );
    let session = json(viewer(&restarted, &token, "/api/shared/session").await).await;
    assert_eq!(session["segment"], "Chrome");

    // Revocation also survives.
    admin(
        &restarted,
        "DELETE",
        &format!("/api/share-links/{id}"),
        None,
    )
    .await;
    let third = env_with(state_builder().share(ShareState::load(data.path())));
    assert_eq!(
        viewer(&third, &token, "/api/shared/session").await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn test_a_corrupt_store_authorises_nothing_and_is_kept() {
    let data = tempfile::tempdir().unwrap();
    let env = env_with(state_builder().share(ShareState::load(data.path())));
    let (_, token) = plain_link(&env).await;
    std::fs::write(data.path().join("share_links.json"), b"{{ not json").unwrap();

    let restarted = env_with(state_builder().share(ShareState::load(data.path())));
    assert_eq!(
        viewer(&restarted, &token, "/api/shared/session")
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(std::fs::read_dir(data.path()).unwrap().flatten().any(|e| {
        e.file_name()
            .to_string_lossy()
            .starts_with("share_links.json.corrupt-")
    }));
    // And the instance can issue links again.
    let (_, fresh) = plain_link(&restarted).await;
    assert_eq!(
        viewer(&restarted, &fresh, "/api/shared/session")
            .await
            .status(),
        StatusCode::OK
    );
}

// ── Secrets stay out of logs, metrics and errors ─────────────────────────

#[tokio::test]
async fn test_the_token_never_appears_in_logs_metrics_or_error_bodies() {
    let env = env();
    seed(&env).await;
    let (id, token) = chrome_link(&env).await;
    let body_of_token = token[3..].to_string();
    let uri = format!("/api/shared/stats/main?site_id={SITE}");

    let mut bodies = Vec::new();
    // Normal traffic, every failure mode, and the token pasted into places it
    // does not belong: a path, a query value, and a header of the wrong name.
    bodies.push(text(viewer(&env, &token, &uri).await).await);
    bodies.push(
        text(
            viewer(
                &env,
                &token,
                &format!("/api/shared/stats/main?site_id={OTHER_SITE}"),
            )
            .await,
        )
        .await,
    );
    bodies.push(text(viewer(&env, &token, &format!("{uri}&filters=bogus")).await).await);
    bodies.push(text(viewer(&env, &token, &format!("{uri}&period=bogus")).await).await);
    bodies.push(text(viewer(&env, &token, &format!("{uri}&site_id={token}")).await).await);
    bodies.push(text(bare(&env, "GET", &format!("/s/{token}")).await).await);
    bodies.push(text(bare(&env, "GET", &format!("/api/shared/stats/{token}")).await).await);
    bodies.push(text(bare(&env, "GET", &format!("{uri}&share={token}")).await).await);
    bodies
        .push(text(bare(&env, "GET", &format!("/api/stats/main?site_id=x&t={token}")).await).await);
    bodies.push(
        text(
            send(
                &env,
                Request::builder()
                    .uri(&uri)
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await,
        )
        .await,
    );
    admin(&env, "DELETE", &format!("/api/share-links/{id}"), None).await;
    bodies.push(text(viewer(&env, &token, &uri).await).await);
    bodies.push(text(admin(&env, "GET", "/api/share-links", None).await).await);
    bodies.push(text(bare(&env, "GET", "/metrics").await).await);
    bodies.push(text(bare(&env, "GET", "/health/detailed").await).await);

    for body in &bodies {
        assert!(
            !body.contains(&body_of_token),
            "token in a response body: {body}"
        );
    }
    let log = log_text();
    assert!(
        log.contains("Share link created"),
        "the capture works: {} bytes",
        log.len()
    );
    assert!(log.contains("Share link revoked"));
    assert!(
        !log.contains(&body_of_token),
        "the token was written to the log"
    );
    assert!(
        log.contains("ms_[redacted]"),
        "the token pasted into a URL was recorded redacted, proving the redaction ran"
    );
}

// ── Saved segments ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_segment_crud_over_http() {
    let env = env();
    let created = admin(
        &env,
        "POST",
        "/api/segments",
        Some(serde_json::json!({"site_id": SITE, "name": "Firefox", "filters": " browsers == Firefox "})),
    )
    .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let segment = json(created).await;
    assert_eq!(
        segment["filters"], "browsers==Firefox",
        "stored canonically"
    );
    let id = segment["id"].as_str().unwrap();

    // A read-only key can list (the dashboard dropdown) but not change.
    let listing = send(
        &env,
        Request::builder()
            .uri(format!("/api/segments?site_id={SITE}"))
            .header("authorization", format!("Bearer {READONLY_KEY}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(listing.status(), StatusCode::OK);
    assert_eq!(json(listing).await[0]["name"], "Firefox");
    let none = json(
        admin(
            &env,
            "GET",
            &format!("/api/segments?site_id={OTHER_SITE}"),
            None,
        )
        .await,
    )
    .await;
    assert!(none.as_array().unwrap().is_empty());
    assert_eq!(
        bare(&env, "GET", "/api/segments").await.status(),
        StatusCode::UNAUTHORIZED,
        "listing needs a login once an admin exists"
    );

    let renamed = admin(
        &env,
        "POST",
        &format!("/api/segments/{id}"),
        Some(serde_json::json!({"name": "Firefox users"})),
    )
    .await;
    assert_eq!(json(renamed).await["name"], "Firefox users");
    let empty_edit = admin(
        &env,
        "POST",
        &format!("/api/segments/{id}"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(empty_edit.status(), StatusCode::BAD_REQUEST);
    let unknown = admin(
        &env,
        "POST",
        "/api/segments/nope",
        Some(serde_json::json!({"name": "x"})),
    )
    .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);

    assert_eq!(
        admin(&env, "DELETE", &format!("/api/segments/{id}"), None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        admin(&env, "DELETE", &format!("/api/segments/{id}"), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_segments_reject_what_the_filter_parser_rejects() {
    let env = env();
    let create = |name: &'static str, filters: String| {
        let env = &env;
        async move {
            admin(
                env,
                "POST",
                "/api/segments",
                Some(serde_json::json!({"site_id": SITE, "name": name, "filters": filters})),
            )
            .await
            .status()
        }
    };
    for bad in [
        String::new(),
        "garbage".to_string(),
        "bogus==x".to_string(),
        "browsers==".to_string(),
        "entry-pages==/x".to_string(),
        format!("pages=={}", "x".repeat(513)),
        (0..9)
            .map(|i| format!("pages==/p{i}"))
            .collect::<Vec<_>>()
            .join(";"),
    ] {
        assert_eq!(
            create("n", bad.clone()).await,
            StatusCode::BAD_REQUEST,
            "{bad:?}"
        );
    }
    // Whatever the segment API accepts, the stats API accepts too.
    assert_eq!(
        create("ok", "browsers==Chrome;countries!=US".into()).await,
        StatusCode::CREATED
    );
    assert_eq!(
        create("ok", "browsers==Firefox".into()).await,
        StatusCode::CONFLICT,
        "name taken"
    );
    assert_eq!(
        create("", "browsers==Chrome".into()).await,
        StatusCode::BAD_REQUEST
    );
    let stats = admin(
        &env,
        "GET",
        &format!("/api/stats/main?site_id={SITE}&filters=browsers%3D%3DChrome%3Bcountries!%3DUS"),
        None,
    )
    .await;
    assert_eq!(stats.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_segments_are_capped_per_site() {
    let env = env();
    for i in 0..mallard_metrics::api::segments::MAX_SEGMENTS_PER_SITE {
        let response = admin(
            &env,
            "POST",
            "/api/segments",
            Some(serde_json::json!({"site_id": SITE, "name": format!("s{i}"), "filters": "browsers==Chrome"})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }
    let over = admin(
        &env,
        "POST",
        "/api/segments",
        Some(serde_json::json!({"site_id": SITE, "name": "extra", "filters": "browsers==Chrome"})),
    )
    .await;
    assert_eq!(over.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn test_a_saved_segment_narrows_an_ordinary_dashboard_view() {
    // What the dashboard's dropdown does: apply the saved text as `filters`.
    let env = env();
    seed(&env).await;
    let segment = json(
        admin(
            &env,
            "POST",
            "/api/segments",
            Some(
                serde_json::json!({"site_id": SITE, "name": "FF", "filters": "browsers==Firefox"}),
            ),
        )
        .await,
    )
    .await;
    let filters = segment["filters"].as_str().unwrap().replace("==", "%3D%3D");
    let main = json(
        admin(
            &env,
            "GET",
            &format!("/api/stats/main?site_id={SITE}&period=30d&filters={filters}"),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(main["total_pageviews"], 1);
}

// A segment on a share link is a confidentiality boundary, so it must cut events
// before sessions form. Selecting whole sessions that merely touch the segment
// (what the same filter does for the person who built it) showed a viewer pages
// the segment excludes.
#[tokio::test]
async fn test_a_segment_does_not_leak_the_rest_of_a_visit() {
    let env = env();
    if !require_behavioral(&env.state, "segment confidentiality") {
        return;
    }
    let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/120 Safari/537.36";
    for path in ["/p1", "/p2", "/p3"] {
        event(&env, ua, SITE, "pageview", path, None).await;
    }
    env.state.buffer.flush().expect("flush");
    let segment = admin(
        &env,
        "POST",
        "/api/segments",
        Some(serde_json::json!({"site_id": SITE, "name": "p1", "filters": "pages==/p1"})),
    )
    .await;
    assert_eq!(segment.status(), StatusCode::CREATED);
    let segment_id = json(segment).await["id"].as_str().unwrap().to_string();
    let (_, token) = make_link(
        &env,
        serde_json::json!({"label": "p1 only", "site_id": SITE, "segment_id": segment_id}),
    )
    .await;

    let get = |route: &str| {
        let uri = format!("/api/shared/stats/{route}?site_id={SITE}&period=30d");
        let (env, token) = (&env, token.clone());
        async move { json(viewer(env, &token, &uri).await).await }
    };

    let main = get("main").await;
    assert_eq!(main["total_pageviews"], 1, "{main}");
    assert_eq!(
        main["views_per_visit"], 1.0,
        "the visit's other pages are outside the segment: {main}"
    );
    let exits = get("breakdown/exit-pages").await;
    assert_eq!(
        paths(&exits),
        vec!["/p1"],
        "no page outside the segment: {exits}"
    );
    let entries = get("breakdown/entry-pages").await;
    assert_eq!(paths(&entries), vec!["/p1"], "{entries}");
    assert_eq!(entries[0]["pageviews"], 1, "{entries}");
    let sessions = get("sessions").await;
    assert_eq!(sessions["avg_pages_per_session"], 1.0, "{sessions}");

    // The owner's reading of the very same filter is unchanged, and the two must
    // not share a cache entry: the share request above has already populated it.
    let owner = admin(
        &env,
        "GET",
        &format!("/api/stats/main?site_id={SITE}&period=30d&filters=pages%3D%3D%2Fp1"),
        None,
    )
    .await;
    let owner = json(owner).await;
    assert_eq!(
        owner["views_per_visit"], 3.0,
        "for the person who built the segment, a filter still selects whole visits: {owner}"
    );
}
