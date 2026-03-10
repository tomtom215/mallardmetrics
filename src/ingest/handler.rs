use crate::api::auth::{ApiKeyStore, LoginAttemptTracker, SessionStore};
use crate::ingest::buffer::{Event, EventBuffer};
use crate::ingest::geoip::GeoIpReader;
use crate::ingest::useragent;
use crate::ingest::visitor_id;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Timelike, Utc};
use serde::Deserialize;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

/// Strip query string and fragment from a URL for privacy-preserving referrer storage.
///
/// `https://google.com/search?q=cancer+diagnosis#result` → `https://google.com/search`
pub fn strip_url_query_and_fragment(url: &str) -> &str {
    let url = url.split('?').next().unwrap_or(url);
    url.split('#').next().unwrap_or(url)
}

/// Round a UTC datetime down to the start of its hour.
///
/// Reduces fingerprinting by lowering timestamp precision from milliseconds to hours.
pub fn round_to_hour(dt: DateTime<Utc>) -> chrono::NaiveDateTime {
    dt.with_minute(0)
        .and_then(|t: DateTime<Utc>| t.with_second(0))
        .and_then(|t: DateTime<Utc>| t.with_nanosecond(0))
        .unwrap_or(dt)
        .naive_utc()
}

/// UTM parameters tuple: (source, medium, campaign, content, term).
type UtmParams = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Inbound event payload from the tracking script.
#[derive(Debug, Deserialize)]
pub struct EventPayload {
    /// Site domain (e.g., "example.com")
    #[serde(rename = "d")]
    pub domain: String,
    /// Event name (e.g., "pageview")
    #[serde(rename = "n")]
    pub name: String,
    /// Page URL pathname
    #[serde(rename = "u")]
    pub url: String,
    /// Referrer URL
    #[serde(rename = "r")]
    pub referrer: Option<String>,
    /// Screen width
    #[serde(rename = "w")]
    pub screen_width: Option<u32>,
    /// Custom properties (JSON string)
    #[serde(rename = "p")]
    pub props: Option<String>,
    /// Revenue amount
    #[serde(rename = "ra")]
    pub revenue_amount: Option<f64>,
    /// Revenue currency
    #[serde(rename = "rc")]
    pub revenue_currency: Option<String>,
}

/// Shared application state.
#[allow(clippy::struct_excessive_bools)]
pub struct AppState {
    pub buffer: EventBuffer,
    pub secret: String,
    pub allowed_sites: Vec<String>,
    pub geoip: GeoIpReader,
    pub filter_bots: bool,
    pub sessions: SessionStore,
    pub api_keys: ApiKeyStore,
    /// Hashed admin password (Argon2id). None if no admin user set up yet.
    pub admin_password_hash: parking_lot::Mutex<Option<String>>,
    pub dashboard_origin: Option<String>,
    pub query_cache: crate::query::cache::QueryCache,
    pub rate_limiter: crate::ingest::ratelimit::RateLimiter,
    /// Per-IP login attempt tracker for brute-force protection.
    pub login_attempt_tracker: LoginAttemptTracker,
    /// Running total of events successfully buffered since startup.
    pub events_ingested_total: Arc<AtomicU64>,
    /// Running total of Parquet flush failures since startup.
    pub flush_failures_total: Arc<AtomicU64>,
    /// Running total of rate-limited ingest requests since startup.
    pub rate_limit_rejections_total: Arc<AtomicU64>,
    /// Running total of failed login attempts since startup.
    pub login_failures_total: Arc<AtomicU64>,
    /// Optional bearer token required to access the `/metrics` endpoint.
    /// `None` means the endpoint is accessible without authentication.
    pub metrics_token: Option<String>,
    /// Semaphore limiting the number of concurrent expensive analytics queries.
    /// A permit is acquired before entering `spawn_blocking` for stats endpoints.
    /// Prevents a tight query loop from monopolising the single DuckDB connection.
    pub query_semaphore: Arc<tokio::sync::Semaphore>,
    /// Force the `Secure` flag on session cookies.
    /// Set via `MALLARD_SECURE_COOKIES=true` when behind a TLS-terminating proxy.
    pub secure_cookies: bool,
    /// Whether the DuckDB `behavioral` extension was successfully loaded at startup.
    /// Exposed in `/health/detailed` and the Prometheus `/metrics` endpoint.
    pub behavioral_extension_loaded: bool,

    // ── Privacy / GDPR configuration ─────────────────────────────────────
    /// Strip query string and fragment from referrer URLs before storing.
    pub strip_referrer_query: bool,
    /// Round event timestamps to the nearest hour.
    pub round_timestamps: bool,
    /// Replace the HMAC visitor_id with a random UUID per request (breaks cross-request linking).
    pub suppress_visitor_id: bool,
    /// Omit browser version (store browser name only).
    pub suppress_browser_version: bool,
    /// Omit OS version (store OS name only).
    pub suppress_os_version: bool,
    /// Omit screen_size and device_type fields.
    pub suppress_screen_size: bool,
    /// GeoIP precision: "city" | "region" | "country" | "none".
    pub geoip_precision: String,
    /// Path to the events directory; needed by the GDPR erasure endpoint.
    pub events_dir: std::path::PathBuf,
}

/// Query parameters for the GET /api/event pixel-tracking endpoint.
///
/// Subset of `EventPayload` — props and revenue fields are omitted because
/// they cannot be safely validated in a plain query string.
#[derive(Debug, Deserialize)]
pub struct PixelParams {
    /// Site domain (e.g., "example.com")
    #[serde(rename = "d")]
    pub domain: String,
    /// Event name (defaults to "pageview")
    #[serde(rename = "n", default = "default_event_name")]
    pub name: String,
    /// Page URL pathname or full URL
    #[serde(rename = "u")]
    pub url: String,
    /// Referrer URL
    #[serde(rename = "r")]
    pub referrer: Option<String>,
    /// Screen width in pixels
    #[serde(rename = "w")]
    pub screen_width: Option<u32>,
}

fn default_event_name() -> String {
    "pageview".to_string()
}

/// Shared event-processing logic called by both the POST and GET endpoints.
///
/// Validates the payload, applies rate limiting, builds an `Event`, and pushes
/// it into the buffer.  Returns `false` if the event should be silently ignored
/// (bot filtered, origin blocked, rate limited).
#[allow(clippy::too_many_lines)]
pub async fn process_pixel_event(state: &Arc<AppState>, headers: &HeaderMap, params: PixelParams) {
    // Convert PixelParams into the canonical EventPayload shape so we can
    // call the same validation / enrichment path.
    let payload = EventPayload {
        domain: params.domain,
        name: params.name,
        url: params.url,
        referrer: params.referrer,
        screen_width: params.screen_width,
        props: None,
        revenue_amount: None,
        revenue_currency: None,
    };

    // Reuse the same guard sequence as ingest_event: origin, basic validation,
    // length, site_id char-set, rate limit, bot filter.
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    if !crate::api::auth::validate_origin(origin, &state.allowed_sites) {
        return;
    }
    if payload.domain.is_empty() || payload.name.is_empty() || payload.url.is_empty() {
        return;
    }
    if payload.domain.len() > 256
        || payload.name.len() > 256
        || payload.url.len() > 2048
        || payload.referrer.as_ref().is_some_and(|r| r.len() > 2048)
    {
        return;
    }
    if crate::api::stats::validate_site_id(&payload.domain).is_err() {
        return;
    }
    if !state.rate_limiter.check(&payload.domain) {
        state
            .rate_limit_rejections_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return;
    }

    let ip = extract_ip(headers);
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let parsed_ua = useragent::parse_user_agent(user_agent);
    if state.filter_bots && parsed_ua.is_bot {
        return;
    }

    let today = Utc::now().date_naive();
    let salt = visitor_id::daily_salt(&state.secret, today);
    // Privacy: suppress_visitor_id replaces the deterministic HMAC with a random UUID
    // so that no cross-request linkability is possible.
    let vid = if state.suppress_visitor_id {
        uuid::Uuid::new_v4().to_string()
    } else {
        visitor_id::generate_visitor_id(&ip, user_agent, &salt)
    };
    let (utm_source, utm_medium, utm_campaign, utm_content, utm_term) =
        parse_utm_params(&payload.url);
    let referrer_source = payload
        .referrer
        .as_deref()
        .and_then(extract_referrer_source);
    let geo_info = state.geoip.lookup(&ip);
    // Privacy: apply geoip_precision — strip city/region fields as configured.
    let (country_code, region, city) = match state.geoip_precision.as_str() {
        "none" => (None, None, None),
        "country" => (geo_info.country_code, None, None),
        "region" => (geo_info.country_code, geo_info.region, None),
        _ => (geo_info.country_code, geo_info.region, geo_info.city), // "city" (default)
    };
    // Privacy: suppress_screen_size omits both screen width and derived device type.
    let (screen_size, device_type) = if state.suppress_screen_size {
        (None, None)
    } else {
        (
            payload.screen_width.map(|w| format!("{w}")),
            payload.screen_width.map(classify_device),
        )
    };
    let pathname = sanitize_pathname(&payload.url);
    // Privacy: round_timestamps reduces precision to the nearest hour.
    let timestamp = if state.round_timestamps {
        round_to_hour(Utc::now())
    } else {
        Utc::now().naive_utc()
    };
    // Privacy: strip_referrer_query removes query strings and fragments from referrer URLs.
    let referrer = payload.referrer.as_deref().map(|r| {
        let r = if state.strip_referrer_query {
            strip_url_query_and_fragment(r)
        } else {
            r
        };
        sanitize_string(r, 2048)
    });
    // Privacy: suppress_browser_version / suppress_os_version reduce fingerprinting surface.
    let browser_version = if state.suppress_browser_version {
        None
    } else {
        parsed_ua.browser_version
    };
    let os_version = if state.suppress_os_version {
        None
    } else {
        parsed_ua.os_version
    };

    let event = Event {
        site_id: sanitize_string(&payload.domain, 256),
        visitor_id: vid,
        timestamp,
        event_name: sanitize_string(&payload.name, 256),
        pathname,
        hostname: Some(sanitize_string(&payload.domain, 256)),
        referrer,
        referrer_source,
        utm_source,
        utm_medium,
        utm_campaign,
        utm_content,
        utm_term,
        browser: parsed_ua.browser,
        browser_version,
        os: parsed_ua.os,
        os_version,
        device_type,
        screen_size,
        country_code,
        region,
        city,
        props: None,
        revenue_amount: None,
        revenue_currency: None,
    };

    let state2 = Arc::clone(state);
    match tokio::task::spawn_blocking(move || state2.buffer.push(event)).await {
        Ok(Ok(_)) => {
            state
                .events_ingested_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(Err(e)) => tracing::error!(error = %e, "Failed to buffer pixel event"),
        Err(e) => tracing::error!(error = %e, "Pixel event buffer task panicked"),
    }
}

/// POST /api/event — Ingestion endpoint.
///
/// Receives events from the tracking script, generates a privacy-safe visitor ID,
/// and pushes the event into the buffer.
#[allow(clippy::too_many_lines)]
pub async fn ingest_event(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<EventPayload>,
) -> impl IntoResponse {
    // Validate origin against allowed sites
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    if !crate::api::auth::validate_origin(origin, &state.allowed_sites) {
        return StatusCode::FORBIDDEN;
    }

    // Validate required fields
    if payload.domain.is_empty() || payload.name.is_empty() || payload.url.is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    // Length validation before further processing (and before rate limiting) to
    // prevent allocating resources for clearly oversized inputs.
    if payload.domain.len() > 256
        || payload.name.len() > 256
        || payload.url.len() > 2048
        || payload.referrer.as_ref().is_some_and(|r| r.len() > 2048)
        || payload.props.as_ref().is_some_and(|p| p.len() > 4096)
    {
        return StatusCode::BAD_REQUEST;
    }

    // Character-set validation for domain BEFORE rate limiting.
    //
    // Without this ordering an invalid domain (e.g. "my site.com" with a space)
    // would create a rate-limiter bucket for the invalid string and then return
    // 400 — wasting bucket memory for strings that can never be valid site IDs.
    if crate::api::stats::validate_site_id(&payload.domain).is_err() {
        return StatusCode::BAD_REQUEST;
    }

    // Rate limiting per site (only reached for well-formed site IDs)
    if !state.rate_limiter.check(&payload.domain) {
        state
            .rate_limit_rejections_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return StatusCode::TOO_MANY_REQUESTS;
    }

    // Extract client IP and User-Agent for visitor ID
    let ip = extract_ip(&headers);
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Parse User-Agent for browser/OS information and bot detection
    let parsed_ua = useragent::parse_user_agent(user_agent);

    // Filter bot traffic if configured
    if state.filter_bots && parsed_ua.is_bot {
        return StatusCode::ACCEPTED;
    }

    let today = Utc::now().date_naive();
    let salt = visitor_id::daily_salt(&state.secret, today);
    // Privacy: suppress_visitor_id replaces the deterministic HMAC with a random UUID.
    let vid = if state.suppress_visitor_id {
        uuid::Uuid::new_v4().to_string()
    } else {
        visitor_id::generate_visitor_id(&ip, user_agent, &salt)
    };

    // Parse UTM parameters from URL
    let (utm_source, utm_medium, utm_campaign, utm_content, utm_term) =
        parse_utm_params(&payload.url);

    // Parse referrer source (extract before potential query-strip so the hostname is still present)
    let referrer_source = payload
        .referrer
        .as_deref()
        .and_then(extract_referrer_source);

    // Look up geographic information from IP (PRIVACY: IP used only for lookup, never stored)
    let geo_info = state.geoip.lookup(&ip);
    // Privacy: apply geoip_precision — strip city/region fields as configured.
    let (country_code, region, city) = match state.geoip_precision.as_str() {
        "none" => (None, None, None),
        "country" => (geo_info.country_code, None, None),
        "region" => (geo_info.country_code, geo_info.region, None),
        _ => (geo_info.country_code, geo_info.region, geo_info.city), // "city" (default)
    };

    // Privacy: suppress_screen_size omits both screen width and derived device type.
    let (screen_size, device_type) = if state.suppress_screen_size {
        (None, None)
    } else {
        (
            payload.screen_width.map(|w| format!("{w}")),
            payload.screen_width.map(classify_device),
        )
    };

    // Sanitize pathname
    let pathname = sanitize_pathname(&payload.url);

    // Privacy: round_timestamps reduces precision to the nearest hour.
    let timestamp = if state.round_timestamps {
        round_to_hour(Utc::now())
    } else {
        Utc::now().naive_utc()
    };

    // Privacy: strip_referrer_query removes query strings and fragments from referrer URLs.
    let referrer = payload.referrer.as_deref().map(|r| {
        let r = if state.strip_referrer_query {
            strip_url_query_and_fragment(r)
        } else {
            r
        };
        sanitize_string(r, 2048)
    });

    // Privacy: suppress_browser_version / suppress_os_version reduce fingerprinting surface.
    let browser_version = if state.suppress_browser_version {
        None
    } else {
        parsed_ua.browser_version
    };
    let os_version = if state.suppress_os_version {
        None
    } else {
        parsed_ua.os_version
    };

    let event = Event {
        site_id: sanitize_string(&payload.domain, 256),
        visitor_id: vid,
        timestamp,
        event_name: sanitize_string(&payload.name, 256),
        pathname,
        hostname: Some(sanitize_string(&payload.domain, 256)),
        referrer,
        referrer_source,
        utm_source,
        utm_medium,
        utm_campaign,
        utm_content,
        utm_term,
        browser: parsed_ua.browser,
        browser_version,
        os: parsed_ua.os,
        os_version,
        device_type,
        screen_size,
        country_code,
        region,
        city,
        props: payload.props.as_deref().map(|p| sanitize_string(p, 4096)),
        revenue_amount: payload.revenue_amount,
        revenue_currency: payload
            .revenue_currency
            .as_deref()
            .map(|c| sanitize_string(c, 3)),
    };

    // Push the event on a blocking thread so that a threshold-triggered flush
    // (which acquires the DuckDB mutex and writes Parquet) does not hold a Tokio
    // worker thread.  The counter is incremented from the async side after the
    // blocking task completes.
    let state2 = Arc::clone(&state);
    match tokio::task::spawn_blocking(move || state2.buffer.push(event)).await {
        Ok(Ok(_)) => {
            state
                .events_ingested_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            StatusCode::ACCEPTED
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Failed to buffer event");
            StatusCode::INTERNAL_SERVER_ERROR
        }
        Err(e) => {
            tracing::error!(error = %e, "Event buffer task panicked");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Extract client IP from headers, checking X-Forwarded-For first.
pub fn extract_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .unwrap_or("unknown")
        .to_string()
}

/// Parse UTM parameters from a URL string.
fn parse_utm_params(url: &str) -> UtmParams {
    let query_start = url.find('?');
    let query = match query_start {
        Some(pos) => &url[pos + 1..],
        None => return (None, None, None, None, None),
    };

    let mut source = None;
    let mut medium = None;
    let mut campaign = None;
    let mut content = None;
    let mut term = None;

    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        if value.is_empty() {
            continue;
        }

        match key {
            "utm_source" => source = Some(sanitize_string(value, 256)),
            "utm_medium" => medium = Some(sanitize_string(value, 256)),
            "utm_campaign" => campaign = Some(sanitize_string(value, 256)),
            "utm_content" => content = Some(sanitize_string(value, 256)),
            "utm_term" => term = Some(sanitize_string(value, 256)),
            _ => {}
        }
    }

    (source, medium, campaign, content, term)
}

/// Extract a simplified referrer source name from a referrer URL.
fn extract_referrer_source(referrer: &str) -> Option<String> {
    if referrer.is_empty() {
        return None;
    }

    // Extract hostname from referrer
    let host = referrer
        .strip_prefix("https://")
        .or_else(|| referrer.strip_prefix("http://"))
        .unwrap_or(referrer)
        .split('/')
        .next()
        .unwrap_or(referrer)
        .split(':')
        .next()
        .unwrap_or(referrer);

    if host.is_empty() {
        return None;
    }

    // Map known referrers to source names
    let source = if host.contains("google") {
        "Google"
    } else if host.contains("bing") {
        "Bing"
    } else if host.contains("yahoo") {
        "Yahoo"
    } else if host.contains("duckduckgo") {
        "DuckDuckGo"
    } else if host.contains("twitter") || host == "t.co" {
        "Twitter"
    } else if host.contains("facebook") || host.contains("fb.com") {
        "Facebook"
    } else if host.contains("linkedin") {
        "LinkedIn"
    } else if host.contains("reddit") {
        "Reddit"
    } else if host.contains("github") {
        "GitHub"
    } else {
        host
    };

    Some(source.to_string())
}

/// Classify device type based on screen width.
fn classify_device(width: u32) -> String {
    if width < 768 {
        "mobile".to_string()
    } else if width < 1024 {
        "tablet".to_string()
    } else {
        "desktop".to_string()
    }
}

/// Extract pathname from URL, stripping query string and fragment.
fn sanitize_pathname(url: &str) -> String {
    let path = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    let path = path.split('/').skip(1).collect::<Vec<_>>().join("/");
    let path = format!("/{path}");

    // Remove query string and fragment
    let path = path.split('?').next().unwrap_or(&path);
    let path = path.split('#').next().unwrap_or(path);

    sanitize_string(path, 2048)
}

/// Sanitize a string by truncating to max length and removing control characters.
fn sanitize_string(input: &str, max_len: usize) -> String {
    input
        .chars()
        .filter(|c| !c.is_control())
        .take(max_len)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_ip_from_x_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 5.6.7.8".parse().unwrap());
        assert_eq!(extract_ip(&headers), "1.2.3.4");
    }

    #[test]
    fn test_extract_ip_from_x_real_ip() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "1.2.3.4".parse().unwrap());
        assert_eq!(extract_ip(&headers), "1.2.3.4");
    }

    #[test]
    fn test_extract_ip_unknown() {
        let headers = HeaderMap::new();
        assert_eq!(extract_ip(&headers), "unknown");
    }

    #[test]
    fn test_parse_utm_params() {
        let url = "https://example.com/page?utm_source=google&utm_medium=cpc&utm_campaign=winter&utm_content=banner&utm_term=analytics";
        let (source, medium, campaign, content, term) = parse_utm_params(url);
        assert_eq!(source.unwrap(), "google");
        assert_eq!(medium.unwrap(), "cpc");
        assert_eq!(campaign.unwrap(), "winter");
        assert_eq!(content.unwrap(), "banner");
        assert_eq!(term.unwrap(), "analytics");
    }

    #[test]
    fn test_parse_utm_params_none() {
        let url = "https://example.com/page";
        let (source, medium, campaign, content, term) = parse_utm_params(url);
        assert!(source.is_none());
        assert!(medium.is_none());
        assert!(campaign.is_none());
        assert!(content.is_none());
        assert!(term.is_none());
    }

    #[test]
    fn test_parse_utm_partial() {
        let url = "https://example.com/?utm_source=google";
        let (source, medium, _, _, _) = parse_utm_params(url);
        assert_eq!(source.unwrap(), "google");
        assert!(medium.is_none());
    }

    #[test]
    fn test_extract_referrer_source_google() {
        assert_eq!(
            extract_referrer_source("https://www.google.com/search?q=test"),
            Some("Google".to_string())
        );
    }

    #[test]
    fn test_extract_referrer_source_unknown() {
        assert_eq!(
            extract_referrer_source("https://myblog.com/post"),
            Some("myblog.com".to_string())
        );
    }

    #[test]
    fn test_extract_referrer_source_empty() {
        assert_eq!(extract_referrer_source(""), None);
    }

    #[test]
    fn test_classify_device_mobile() {
        assert_eq!(classify_device(375), "mobile");
    }

    #[test]
    fn test_classify_device_tablet() {
        assert_eq!(classify_device(768), "tablet");
    }

    #[test]
    fn test_classify_device_desktop() {
        assert_eq!(classify_device(1920), "desktop");
    }

    #[test]
    fn test_sanitize_pathname() {
        assert_eq!(
            sanitize_pathname("https://example.com/about?ref=1#section"),
            "/about"
        );
    }

    #[test]
    fn test_sanitize_pathname_root() {
        assert_eq!(sanitize_pathname("https://example.com/"), "/");
    }

    #[test]
    fn test_sanitize_pathname_deep() {
        assert_eq!(
            sanitize_pathname("https://example.com/blog/post/123"),
            "/blog/post/123"
        );
    }

    #[test]
    fn test_strip_url_query_and_fragment_query() {
        assert_eq!(
            strip_url_query_and_fragment("https://google.com/search?q=cancer+diagnosis"),
            "https://google.com/search"
        );
    }

    #[test]
    fn test_strip_url_query_and_fragment_fragment() {
        assert_eq!(
            strip_url_query_and_fragment("https://example.com/page#section"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_strip_url_query_and_fragment_both() {
        assert_eq!(
            strip_url_query_and_fragment("https://example.com/page?a=1#s"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_strip_url_query_and_fragment_no_change() {
        assert_eq!(
            strip_url_query_and_fragment("https://example.com/page"),
            "https://example.com/page"
        );
    }

    #[test]
    fn test_round_to_hour_truncates_minutes_seconds() {
        let dt = chrono::DateTime::parse_from_rfc3339("2024-03-15T14:37:22Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let rounded = round_to_hour(dt);
        assert_eq!(rounded.format("%H:%M:%S").to_string(), "14:00:00");
        assert_eq!(rounded.format("%Y-%m-%d").to_string(), "2024-03-15");
    }

    #[test]
    fn test_round_to_hour_on_exact_hour() {
        let dt = chrono::DateTime::parse_from_rfc3339("2024-03-15T14:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let rounded = round_to_hour(dt);
        assert_eq!(rounded.format("%H:%M:%S").to_string(), "14:00:00");
    }

    #[test]
    fn test_sanitize_string_truncate() {
        let long = "a".repeat(500);
        let result = sanitize_string(&long, 256);
        assert_eq!(result.len(), 256);
    }

    #[test]
    fn test_sanitize_string_control_chars() {
        let input = "hello\x00world\x01test";
        assert_eq!(sanitize_string(input, 256), "helloworldtest");
    }

    #[test]
    fn test_referrer_sources() {
        assert_eq!(
            extract_referrer_source("https://t.co/abc"),
            Some("Twitter".to_string())
        );
        assert_eq!(
            extract_referrer_source("https://www.facebook.com/"),
            Some("Facebook".to_string())
        );
        assert_eq!(
            extract_referrer_source("https://www.reddit.com/r/rust"),
            Some("Reddit".to_string())
        );
        assert_eq!(
            extract_referrer_source("https://github.com/user/repo"),
            Some("GitHub".to_string())
        );
        assert_eq!(
            extract_referrer_source("https://www.linkedin.com/feed"),
            Some("LinkedIn".to_string())
        );
        assert_eq!(
            extract_referrer_source("https://duckduckgo.com/?q=test"),
            Some("DuckDuckGo".to_string())
        );
    }
}
