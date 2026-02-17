use crate::ingest::buffer::{Event, EventBuffer};
use crate::ingest::visitor_id;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use chrono::Utc;
use serde::Deserialize;
use std::sync::Arc;

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

/// Shared application state for the ingestion handler.
pub struct AppState {
    pub buffer: EventBuffer,
    pub secret: String,
}

/// POST /api/event — Ingestion endpoint.
///
/// Receives events from the tracking script, generates a privacy-safe visitor ID,
/// and pushes the event into the buffer.
pub async fn ingest_event(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(payload): Json<EventPayload>,
) -> impl IntoResponse {
    // Validate required fields
    if payload.domain.is_empty() || payload.name.is_empty() || payload.url.is_empty() {
        return StatusCode::BAD_REQUEST;
    }

    // Length validation to prevent abuse
    if payload.domain.len() > 256
        || payload.name.len() > 256
        || payload.url.len() > 2048
        || payload.referrer.as_ref().is_some_and(|r| r.len() > 2048)
        || payload.props.as_ref().is_some_and(|p| p.len() > 4096)
    {
        return StatusCode::BAD_REQUEST;
    }

    // Extract client IP and User-Agent for visitor ID
    let ip = extract_ip(&headers);
    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let today = Utc::now().date_naive();
    let salt = visitor_id::daily_salt(&state.secret, today);
    let vid = visitor_id::generate_visitor_id(&ip, user_agent, &salt);

    // Parse UTM parameters from URL
    let (utm_source, utm_medium, utm_campaign, utm_content, utm_term) =
        parse_utm_params(&payload.url);

    // Parse referrer source
    let referrer_source = payload
        .referrer
        .as_deref()
        .and_then(extract_referrer_source);

    // Determine device type from screen width
    let device_type = payload.screen_width.map(classify_device);

    // Sanitize pathname
    let pathname = sanitize_pathname(&payload.url);

    let event = Event {
        site_id: sanitize_string(&payload.domain, 256),
        visitor_id: vid,
        timestamp: Utc::now().naive_utc(),
        event_name: sanitize_string(&payload.name, 256),
        pathname,
        hostname: Some(sanitize_string(&payload.domain, 256)),
        referrer: payload
            .referrer
            .as_deref()
            .map(|r| sanitize_string(r, 2048)),
        referrer_source,
        utm_source,
        utm_medium,
        utm_campaign,
        utm_content,
        utm_term,
        browser: None,
        browser_version: None,
        os: None,
        os_version: None,
        device_type,
        screen_size: payload.screen_width.map(|w| format!("{w}")),
        country_code: None,
        region: None,
        city: None,
        props: payload.props.as_deref().map(|p| sanitize_string(p, 4096)),
        revenue_amount: payload.revenue_amount,
        revenue_currency: payload
            .revenue_currency
            .as_deref()
            .map(|c| sanitize_string(c, 3)),
    };

    match state.buffer.push(event) {
        Ok(_) => StatusCode::ACCEPTED,
        Err(e) => {
            tracing::error!(error = %e, "Failed to buffer event");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Extract client IP from headers, checking X-Forwarded-For first.
fn extract_ip(headers: &HeaderMap) -> String {
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
