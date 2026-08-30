use crate::api::auth::{ApiKeyStore, LoginAttemptTracker, SessionStore};
use crate::ingest::buffer::{Event, EventBuffer};
use crate::ingest::geoip::GeoIpReader;
use crate::ingest::useragent::{self, ClientHints};
use crate::ingest::visitor_id;
use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use chrono::{DateTime, Timelike, Utc};
use serde::Deserialize;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Maximum accepted lengths for inbound string fields.
const MAX_DOMAIN_LEN: usize = 256;
const MAX_EVENT_NAME_LEN: usize = 256;
const MAX_URL_LEN: usize = 2048;
const MAX_REFERRER_LEN: usize = 2048;
const MAX_PROPS_LEN: usize = 4096;

/// Strip the query string and fragment from a URL.
///
/// `https://google.com/search?q=cancer+diagnosis#result` → `https://google.com/search`
pub fn strip_url_query_and_fragment(url: &str) -> &str {
    let url = url.split('?').next().unwrap_or(url);
    url.split('#').next().unwrap_or(url)
}

/// Round a UTC datetime down to the start of its hour.
pub fn round_to_hour(dt: DateTime<Utc>) -> chrono::NaiveDateTime {
    dt.with_minute(0)
        .and_then(|t: DateTime<Utc>| t.with_second(0))
        .and_then(|t: DateTime<Utc>| t.with_nanosecond(0))
        .unwrap_or(dt)
        .naive_utc()
}

/// UTM parameters: source, medium, campaign, content, term.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct UtmParams {
    pub source: Option<String>,
    pub medium: Option<String>,
    pub campaign: Option<String>,
    pub content: Option<String>,
    pub term: Option<String>,
}

/// Inbound event payload from the tracking script.
#[derive(Debug, Deserialize)]
pub struct EventPayload {
    /// Site domain, e.g. `example.com`.
    #[serde(rename = "d")]
    pub domain: String,
    /// Event name, e.g. `pageview`.
    #[serde(rename = "n")]
    pub name: String,
    /// Page URL.
    #[serde(rename = "u")]
    pub url: String,
    /// Referrer URL.
    #[serde(rename = "r")]
    pub referrer: Option<String>,
    /// Screen width in CSS pixels.
    #[serde(rename = "w")]
    pub screen_width: Option<u32>,
    /// Custom properties, as a JSON object string.
    #[serde(rename = "p")]
    pub props: Option<String>,
    /// Revenue amount.
    #[serde(rename = "ra")]
    pub revenue_amount: Option<f64>,
    /// Revenue currency (ISO 4217).
    #[serde(rename = "rc")]
    pub revenue_currency: Option<String>,
}

/// Query parameters for the `GET /api/event` pixel endpoint.
///
/// A subset of [`EventPayload`]: props and revenue are omitted because a plain
/// query string cannot carry them safely.
#[derive(Debug, Deserialize)]
pub struct PixelParams {
    #[serde(rename = "d")]
    pub domain: String,
    #[serde(rename = "n", default = "default_event_name")]
    pub name: String,
    #[serde(rename = "u")]
    pub url: String,
    #[serde(rename = "r")]
    pub referrer: Option<String>,
    #[serde(rename = "w")]
    pub screen_width: Option<u32>,
}

fn default_event_name() -> String {
    "pageview".to_string()
}

impl From<PixelParams> for EventPayload {
    fn from(p: PixelParams) -> Self {
        Self {
            domain: p.domain,
            name: p.name,
            url: p.url,
            referrer: p.referrer,
            screen_width: p.screen_width,
            props: None,
            revenue_amount: None,
            revenue_currency: None,
        }
    }
}

/// Shared application state.
#[allow(clippy::struct_excessive_bools)]
pub struct AppState {
    pub buffer: EventBuffer,
    /// Read-only DuckDB connections used to serve analytics queries.
    pub readers: crate::storage::ReaderPool,
    pub secret: String,
    pub allowed_sites: Vec<String>,
    pub geoip: GeoIpReader,
    pub filter_bots: bool,
    pub sessions: SessionStore,
    pub api_keys: ApiKeyStore,
    /// Hashed admin password (Argon2id). `None` until setup runs.
    pub admin_password_hash: parking_lot::Mutex<Option<String>>,
    pub dashboard_origin: Option<String>,
    pub query_cache: crate::query::cache::QueryCache,
    /// Per-site ingest rate limiter.
    pub rate_limiter: crate::ingest::ratelimit::RateLimiter,
    /// Per-client-IP ingest rate limiter.
    pub ip_rate_limiter: crate::ingest::ratelimit::RateLimiter,
    pub login_attempt_tracker: LoginAttemptTracker,
    pub events_ingested_total: Arc<AtomicU64>,
    pub flush_failures_total: Arc<AtomicU64>,
    pub rate_limit_rejections_total: Arc<AtomicU64>,
    pub login_failures_total: Arc<AtomicU64>,
    /// Bearer token required by `/metrics`. `None` leaves it open.
    pub metrics_token: Option<String>,
    /// Caps concurrent analytics queries.
    pub query_semaphore: Arc<tokio::sync::Semaphore>,
    pub secure_cookies: bool,
    /// Whether the DuckDB `behavioral` extension loaded at startup.
    pub behavioral_extension_loaded: bool,
    /// Version reported by the behavioral extension, when loaded.
    pub behavioral_version: Option<String>,
    /// Trust `X-Forwarded-For` / `X-Real-IP` for the client address.
    pub trust_proxy_headers: bool,
    /// Session inactivity window, as a DuckDB interval literal.
    pub session_window: String,
    /// Window treated as "now" by the realtime endpoint.
    pub realtime_window_minutes: u32,
    /// Days a visitor-ID salt stays valid.
    pub visitor_salt_rotation_days: u32,

    // ── Privacy / GDPR configuration ─────────────────────────────────────
    pub strip_referrer_query: bool,
    pub round_timestamps: bool,
    pub suppress_visitor_id: bool,
    pub suppress_browser_version: bool,
    pub suppress_os_version: bool,
    pub suppress_screen_size: bool,
    /// `city` | `region` | `country` | `none`.
    pub geoip_precision: String,
    /// Events directory, needed by the GDPR erasure endpoint.
    pub events_dir: std::path::PathBuf,
}

impl AppState {
    /// A [`QueryScope`](crate::query::QueryScope) for this deployment's session window.
    pub fn scope(&self, site_id: &str, start: &str, end: &str) -> crate::query::QueryScope {
        crate::query::QueryScope::new(site_id, start, end, self.session_window.clone())
    }
}

/// Why an event was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// Origin header not in the configured allowlist.
    Origin,
    /// A required field was empty, or a field exceeded its length limit.
    Invalid,
    /// `site_id` failed character-set validation.
    SiteId,
    /// `site_id` is not in the configured allowlist.
    SiteNotAllowed,
    /// The per-site or per-IP rate limit was exceeded.
    RateLimited,
    /// Recognised as bot traffic while bot filtering is on.
    Bot,
}

impl RejectReason {
    /// HTTP status for the `POST` endpoint.
    pub const fn status(self) -> StatusCode {
        match self {
            Self::Origin | Self::SiteNotAllowed => StatusCode::FORBIDDEN,
            Self::Invalid | Self::SiteId => StatusCode::BAD_REQUEST,
            Self::RateLimited => StatusCode::TOO_MANY_REQUESTS,
            // Bot traffic is accepted-and-dropped so crawlers do not retry.
            Self::Bot => StatusCode::ACCEPTED,
        }
    }
}

/// The peer socket address, when the server was started with `ConnectInfo`.
///
/// A plain `Option<ConnectInfo<SocketAddr>>` is not an extractor axum accepts
/// here, and a bare `ConnectInfo<SocketAddr>` would make every handler fail in
/// tests, which drive the router with `oneshot` and never install it. Reading
/// the extension directly is infallible and works in both settings.
#[derive(Debug, Clone, Copy)]
pub struct PeerAddr(pub Option<SocketAddr>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> {
        // Reading an extension needs no I/O, so the future resolves immediately
        // rather than allocating an async state machine for nothing.
        std::future::ready(Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(addr)| *addr),
        )))
    }
}

/// Resolve the client IP for a request.
///
/// When `trust_proxy_headers` is off (the default), the peer socket address is
/// used. `X-Forwarded-For` and `X-Real-IP` are attacker-controlled on a directly
/// reachable server: trusting them unconditionally let any client choose its own
/// visitor ID, geolocation and rate-limit bucket.
///
/// Falling back to the peer address also fixes a quieter problem. Without a
/// proxy, neither header is present, and the old code returned the literal
/// string `"unknown"` for every request — so every visitor sharing a
/// User-Agent collapsed into a single visitor ID, and unique-visitor counts on
/// non-proxied deployments were meaningless.
pub fn client_ip(state: &AppState, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if state.trust_proxy_headers {
        // The leftmost entry is the original client; entries to its right were
        // added by intermediate proxies.
        if let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .and_then(parse_forwarded_addr)
        {
            return forwarded;
        }
        if let Some(real_ip) = headers
            .get("x-real-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_forwarded_addr)
        {
            return real_ip;
        }
    }
    peer.map_or_else(|| "unknown".to_string(), |addr| addr.ip().to_string())
}

/// Parse one proxy-header entry into a canonical IP address.
///
/// The value is required to be an address rather than passed through verbatim.
/// A trusted header is still attacker-controlled whenever a request reaches the
/// server without traversing the proxy that overwrites it, and this value
/// becomes a rate-limiter map key, a GeoIP lookup input and part of the visitor
/// ID — so arbitrary text of arbitrary length must not get in. Anything that
/// does not parse falls through to the peer address, which cannot be forged.
///
/// Returning the parsed form also canonicalises equivalent spellings, so
/// `::1`, `0:0:0:0:0:0:0:1` and `[::1]:443` cannot masquerade as three
/// different visitors.
fn parse_forwarded_addr(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Ok(ip) = trimmed.parse::<std::net::IpAddr>() {
        return Some(ip.to_string());
    }
    // Some proxies append the source port, and a bracketed IPv6 literal only
    // parses with one.
    if let Ok(addr) = trimmed.parse::<SocketAddr>() {
        return Some(addr.ip().to_string());
    }
    trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|inner| inner.parse::<std::net::Ipv6Addr>().ok())
        .map(|ip| ip.to_string())
}

/// Validate and enrich a payload into a storable [`Event`].
///
/// Shared by the `POST` and pixel paths. Both previously carried their own copy
/// of this logic — about 130 duplicated lines — so a privacy flag fixed in one
/// silently did not apply to the other.
///
/// # Errors
///
/// Returns the reason the event was rejected.
#[allow(clippy::too_many_lines)]
pub fn build_event(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    payload: &EventPayload,
) -> Result<Event, RejectReason> {
    let origin = headers.get("origin").and_then(|v| v.to_str().ok());
    if !crate::api::auth::validate_origin(origin, &state.allowed_sites) {
        return Err(RejectReason::Origin);
    }

    if payload.domain.is_empty() || payload.name.is_empty() || payload.url.is_empty() {
        return Err(RejectReason::Invalid);
    }
    if payload.domain.len() > MAX_DOMAIN_LEN
        || payload.name.len() > MAX_EVENT_NAME_LEN
        || payload.url.len() > MAX_URL_LEN
        || payload
            .referrer
            .as_ref()
            .is_some_and(|r| r.len() > MAX_REFERRER_LEN)
        || payload
            .props
            .as_ref()
            .is_some_and(|p| p.len() > MAX_PROPS_LEN)
    {
        return Err(RejectReason::Invalid);
    }

    // Character-set validation before rate limiting, so an invalid domain never
    // allocates a rate-limiter bucket for a string that can never be a site ID.
    if crate::api::stats::validate_site_id(&payload.domain).is_err() {
        return Err(RejectReason::SiteId);
    }

    // Enforce the allowlist against the payload, not just the Origin header.
    // `Origin` is absent on non-browser requests, so checking it alone left the
    // allowlist trivially bypassable: anyone could POST events for any site_id
    // and create partitions on disk for domains the operator never configured.
    if !state.allowed_sites.is_empty() && !state.allowed_sites.iter().any(|s| s == &payload.domain)
    {
        return Err(RejectReason::SiteNotAllowed);
    }

    let ip = client_ip(state, headers, peer);

    if !state.rate_limiter.check(&payload.domain) || !state.ip_rate_limiter.check(&ip) {
        return Err(RejectReason::RateLimited);
    }

    let user_agent = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let hints = ClientHints::from_headers(headers);
    let parsed_ua = useragent::parse_user_agent_with_hints(user_agent, &hints);
    if state.filter_bots && parsed_ua.is_bot {
        return Err(RejectReason::Bot);
    }

    let now = Utc::now();
    let visitor_id = if state.suppress_visitor_id {
        uuid::Uuid::new_v4().to_string()
    } else {
        let salt = visitor_id::rotating_salt(
            &state.secret,
            now.date_naive(),
            state.visitor_salt_rotation_days,
        );
        visitor_id::generate_visitor_id(&payload.domain, &ip, user_agent, &salt)
    };

    let utm = parse_utm_params(&payload.url);
    // Extracted before any referrer stripping, so the hostname is still present.
    let referrer_source = payload
        .referrer
        .as_deref()
        .and_then(extract_referrer_source);

    let geo = state.geoip.lookup(&ip);
    let (country_code, region, city) = match state.geoip_precision.as_str() {
        "none" => (None, None, None),
        "country" => (geo.country_code, None, None),
        "region" => (geo.country_code, geo.region, None),
        _ => (geo.country_code, geo.region, geo.city),
    };

    let (screen_size, device_type) = if state.suppress_screen_size {
        (None, None)
    } else {
        // Client hints report mobile-ness directly; fall back to screen width.
        let device = hints
            .is_mobile
            .map(|mobile| if mobile { "mobile" } else { "desktop" }.to_string())
            .or_else(|| payload.screen_width.map(classify_device));
        (payload.screen_width.map(|w| w.to_string()), device)
    };

    let timestamp = if state.round_timestamps {
        round_to_hour(now)
    } else {
        now.naive_utc()
    };

    let referrer = payload.referrer.as_deref().map(|r| {
        let r = if state.strip_referrer_query {
            strip_url_query_and_fragment(r)
        } else {
            r
        };
        sanitize_string(r, MAX_REFERRER_LEN)
    });

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

    let site_id = sanitize_string(&payload.domain, MAX_DOMAIN_LEN);

    Ok(Event {
        hostname: Some(site_id.clone()),
        site_id,
        visitor_id,
        timestamp,
        event_name: sanitize_string(&payload.name, MAX_EVENT_NAME_LEN),
        pathname: sanitize_pathname(&payload.url),
        referrer,
        referrer_source,
        utm_source: utm.source,
        utm_medium: utm.medium,
        utm_campaign: utm.campaign,
        utm_content: utm.content,
        utm_term: utm.term,
        browser: parsed_ua.browser,
        browser_version,
        os: parsed_ua.os,
        os_version,
        device_type,
        screen_size,
        country_code,
        region,
        city,
        props: payload.props.as_deref().and_then(sanitize_props),
        revenue_amount: payload.revenue_amount.filter(|a| a.is_finite()),
        revenue_currency: payload
            .revenue_currency
            .as_deref()
            .and_then(normalize_currency),
    })
}

/// Buffer an event and update the ingest counter.
async fn buffer_event(state: &Arc<AppState>, event: Event) -> StatusCode {
    let state2 = Arc::clone(state);
    match tokio::task::spawn_blocking(move || state2.buffer.push(event)).await {
        Ok(Ok(_)) => {
            state.events_ingested_total.fetch_add(1, Ordering::Relaxed);
            StatusCode::ACCEPTED
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Failed to buffer event");
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(e) => {
            tracing::error!(error = %e, "Event buffer task panicked");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Record a rejection against the metrics counters.
fn record_rejection(state: &AppState, reason: RejectReason) {
    if reason == RejectReason::RateLimited {
        state
            .rate_limit_rejections_total
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// POST /api/event — the ingestion endpoint.
pub async fn ingest_event(
    State(state): State<Arc<AppState>>,
    PeerAddr(peer): PeerAddr,
    headers: HeaderMap,
    Json(payload): Json<EventPayload>,
) -> impl IntoResponse {
    match build_event(&state, &headers, peer, &payload) {
        Ok(event) => buffer_event(&state, event).await,
        Err(reason) => {
            record_rejection(&state, reason);
            reason.status()
        }
    }
}

/// Process a pixel-tracker event. Failures are silent: the caller always
/// returns the GIF regardless.
pub async fn process_pixel_event(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    peer: Option<SocketAddr>,
    params: PixelParams,
) {
    let payload: EventPayload = params.into();
    match build_event(state, headers, peer, &payload) {
        Ok(event) => {
            buffer_event(state, event).await;
        }
        Err(reason) => record_rejection(state, reason),
    }
}

/// Percent-decode a URL query component, treating `+` as a space.
///
/// UTM values arrive percent-encoded. Storing them raw meant a campaign named
/// `winter sale` was recorded as `winter%20sale` (or `winter+sale`), so the
/// same campaign split across several rows in the breakdown depending on how
/// each link happened to be encoded.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let decoded = std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                if let Some(byte) = decoded {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // Invalid UTF-8 in a percent escape is replaced rather than dropping the value.
    String::from_utf8_lossy(&out).into_owned()
}

/// Parse UTM parameters out of a URL's query string.
pub fn parse_utm_params(url: &str) -> UtmParams {
    let Some(pos) = url.find('?') else {
        return UtmParams::default();
    };
    // Anything after '#' is a fragment, not part of the query.
    let query = url[pos + 1..].split('#').next().unwrap_or("");

    let mut utm = UtmParams::default();
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");
        if value.is_empty() {
            continue;
        }
        let decoded = sanitize_string(&percent_decode(value), 256);
        if decoded.is_empty() {
            continue;
        }
        // UTM keys are conventionally lowercase but are not case-sensitive in
        // practice; matching case-insensitively avoids losing `UTM_Source=`.
        match key.to_ascii_lowercase().as_str() {
            "utm_source" => utm.source = Some(decoded),
            "utm_medium" => utm.medium = Some(decoded),
            "utm_campaign" => utm.campaign = Some(decoded),
            "utm_content" => utm.content = Some(decoded),
            "utm_term" => utm.term = Some(decoded),
            _ => {}
        }
    }
    utm
}

/// Known referrer hosts mapped to a display name.
///
/// Matched against the registrable-domain suffix rather than a substring:
/// `contains("google")` also matched `not-google.example.com` and
/// `google.phishing.example`, attributing hostile traffic to Google.
const REFERRER_SOURCES: &[(&str, &str)] = &[
    ("google.com", "Google"),
    ("google.co.uk", "Google"),
    ("news.google.com", "Google News"),
    ("bing.com", "Bing"),
    ("yahoo.com", "Yahoo"),
    ("duckduckgo.com", "DuckDuckGo"),
    ("ecosia.org", "Ecosia"),
    ("startpage.com", "Startpage"),
    ("brave.com", "Brave Search"),
    ("t.co", "Twitter"),
    ("twitter.com", "Twitter"),
    ("x.com", "Twitter"),
    ("facebook.com", "Facebook"),
    ("fb.com", "Facebook"),
    ("instagram.com", "Instagram"),
    ("linkedin.com", "LinkedIn"),
    ("lnkd.in", "LinkedIn"),
    ("reddit.com", "Reddit"),
    ("news.ycombinator.com", "Hacker News"),
    ("github.com", "GitHub"),
    ("gitlab.com", "GitLab"),
    ("youtube.com", "YouTube"),
    ("youtu.be", "YouTube"),
    ("pinterest.com", "Pinterest"),
    ("tiktok.com", "TikTok"),
    ("mastodon.social", "Mastodon"),
    ("bsky.app", "Bluesky"),
    ("substack.com", "Substack"),
    ("medium.com", "Medium"),
    ("stackoverflow.com", "Stack Overflow"),
    ("baidu.com", "Baidu"),
    ("yandex.ru", "Yandex"),
    ("naver.com", "Naver"),
];

/// Host portion of a referrer URL, lowercased and without `www.`.
fn referrer_host(referrer: &str) -> Option<String> {
    let host = referrer
        .strip_prefix("https://")
        .or_else(|| referrer.strip_prefix("http://"))
        .unwrap_or(referrer)
        .split('/')
        .next()?
        .split('@')
        .next_back()?
        .split(':')
        .next()?
        .trim()
        .to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(host.strip_prefix("www.").unwrap_or(&host).to_string())
}

/// Map a referrer URL to a friendly source name, or the bare host.
pub fn extract_referrer_source(referrer: &str) -> Option<String> {
    if referrer.is_empty() {
        return None;
    }
    let host = referrer_host(referrer)?;

    for (domain, name) in REFERRER_SOURCES {
        // Exact host, or a subdomain of it. A suffix match alone would let
        // `evilgoogle.com` match `google.com`, so the boundary dot is required.
        if host == *domain || host.ends_with(&format!(".{domain}")) {
            return Some((*name).to_string());
        }
    }
    Some(host)
}

/// Classify a device from its viewport width.
fn classify_device(width: u32) -> String {
    if width < 768 {
        "mobile".to_string()
    } else if width < 1024 {
        "tablet".to_string()
    } else {
        "desktop".to_string()
    }
}

/// Extract a normalised pathname from a URL.
fn sanitize_pathname(url: &str) -> String {
    let without_scheme = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"));

    // An absolute URL loses its authority; a relative one is already a path,
    // which is what the tracker sends when given one.
    let path = without_scheme.map_or(url, |rest| rest.find('/').map_or("/", |idx| &rest[idx..]));

    let path = path.split('?').next().unwrap_or(path);
    let path = path.split('#').next().unwrap_or(path);

    let path = if path.is_empty() { "/" } else { path };
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };

    // Collapse a trailing slash so `/about` and `/about/` are one page rather
    // than two rows that each hold half the traffic. The root stays "/".
    let path = if path.len() > 1 {
        path.trim_end_matches('/').to_string()
    } else {
        path
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else {
        path
    };

    sanitize_string(&path, MAX_URL_LEN)
}

/// Truncate to `max_len` characters and drop control characters.
fn sanitize_string(input: &str, max_len: usize) -> String {
    input
        .chars()
        .filter(|c| !c.is_control())
        .take(max_len)
        .collect()
}

/// Accept custom properties only when they are a JSON object.
///
/// Stored verbatim and later read with DuckDB's JSON functions, so a value that
/// is not an object would make every props query on that site fail. Rejecting
/// it here costs one parse per event and keeps the column well-formed.
fn sanitize_props(props: &str) -> Option<String> {
    let trimmed = props.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    if !value.is_object() {
        return None;
    }
    // Re-serialise so the stored form is canonical rather than however the
    // client happened to format it.
    serde_json::to_string(&value)
        .ok()
        .filter(|s| s.len() <= MAX_PROPS_LEN)
}

/// Normalise a currency to an uppercase ISO 4217 alphabetic code.
fn normalize_currency(code: &str) -> Option<String> {
    let trimmed = code.trim();
    if trimmed.len() != 3 || !trimmed.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(trimmed.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (k, v) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        headers
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    // ── Client IP resolution ─────────────────────────────────────────────

    #[test]
    fn test_client_ip_uses_peer_address_by_default() {
        // Regression: with no proxy configured the old code returned the literal
        // "unknown" for every request, collapsing all direct visitors sharing a
        // User-Agent into a single visitor ID.
        let state = crate::test_support::state_builder().build_state();
        let headers = headers_with(&[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(
            client_ip(&state, &headers, Some(addr("203.0.113.9:5000"))),
            "203.0.113.9"
        );
    }

    #[test]
    fn test_client_ip_ignores_spoofed_headers_when_untrusted() {
        let state = crate::test_support::state_builder().build_state();
        let headers = headers_with(&[("x-real-ip", "9.9.9.9")]);
        assert_eq!(
            client_ip(&state, &headers, Some(addr("198.51.100.4:1234"))),
            "198.51.100.4"
        );
    }

    #[test]
    fn test_client_ip_honours_forwarded_header_when_trusted() {
        let state = crate::test_support::state_builder()
            .trust_proxy_headers(true)
            .build_state();
        let headers = headers_with(&[("x-forwarded-for", "203.0.113.1, 10.0.0.1")]);
        assert_eq!(
            client_ip(&state, &headers, Some(addr("10.0.0.1:80"))),
            "203.0.113.1"
        );
    }

    #[test]
    fn test_client_ip_falls_back_to_real_ip_when_trusted() {
        let state = crate::test_support::state_builder()
            .trust_proxy_headers(true)
            .build_state();
        let headers = headers_with(&[("x-real-ip", "203.0.113.7")]);
        assert_eq!(client_ip(&state, &headers, None), "203.0.113.7");
    }

    #[test]
    fn test_client_ip_ignores_empty_forwarded_header() {
        let state = crate::test_support::state_builder()
            .trust_proxy_headers(true)
            .build_state();
        let headers = headers_with(&[("x-forwarded-for", "")]);
        assert_eq!(
            client_ip(&state, &headers, Some(addr("198.51.100.1:80"))),
            "198.51.100.1"
        );
    }

    #[test]
    fn test_client_ip_rejects_a_proxy_header_that_is_not_an_address() {
        // A trusted header is still attacker-controlled whenever a request can
        // reach the server without traversing the proxy. Arbitrary text would
        // become a rate-limiter key, a GeoIP input and part of a visitor ID.
        let state = crate::test_support::state_builder()
            .trust_proxy_headers(true)
            .build_state();
        for bogus in [
            "not-an-ip",
            "999.999.999.999",
            "'; DROP TABLE events; --",
            "example.com",
            "   ",
        ] {
            let headers = headers_with(&[("x-forwarded-for", bogus)]);
            assert_eq!(
                client_ip(&state, &headers, Some(addr("198.51.100.1:80"))),
                "198.51.100.1",
                "{bogus:?} must fall through to the peer address"
            );
        }
    }

    #[test]
    fn test_client_ip_accepts_a_forwarded_address_with_a_port() {
        // Some proxies append the source port to each entry.
        let state = crate::test_support::state_builder()
            .trust_proxy_headers(true)
            .build_state();
        let headers = headers_with(&[("x-forwarded-for", "203.0.113.5:41234, 10.0.0.1")]);
        assert_eq!(
            client_ip(&state, &headers, Some(addr("10.0.0.1:80"))),
            "203.0.113.5"
        );
    }

    #[test]
    fn test_client_ip_canonicalises_equivalent_ipv6_spellings() {
        // Three spellings of one address must not become three visitors.
        let state = crate::test_support::state_builder()
            .trust_proxy_headers(true)
            .build_state();
        let peer = Some(addr("10.0.0.1:80"));
        let canonical = "2001:db8::1";
        for spelling in [
            "2001:db8::1",
            "2001:0db8:0000:0000:0000:0000:0000:0001",
            "[2001:db8::1]",
            "[2001:db8::1]:8443",
        ] {
            let headers = headers_with(&[("x-forwarded-for", spelling)]);
            assert_eq!(
                client_ip(&state, &headers, peer),
                canonical,
                "{spelling:?} must canonicalise"
            );
        }
    }

    #[test]
    fn test_client_ip_falls_through_to_real_ip_when_forwarded_is_bogus() {
        let state = crate::test_support::state_builder()
            .trust_proxy_headers(true)
            .build_state();
        let headers = headers_with(&[("x-forwarded-for", "garbage"), ("x-real-ip", "203.0.113.7")]);
        assert_eq!(client_ip(&state, &headers, None), "203.0.113.7");
    }

    #[test]
    fn test_client_ip_unknown_without_peer_or_headers() {
        let state = crate::test_support::state_builder().build_state();
        assert_eq!(client_ip(&state, &HeaderMap::new(), None), "unknown");
    }

    // ── UTM parsing ──────────────────────────────────────────────────────

    #[test]
    fn test_parse_utm_params() {
        let utm = parse_utm_params(
            "https://example.com/page?utm_source=google&utm_medium=cpc\
             &utm_campaign=winter&utm_content=banner&utm_term=analytics",
        );
        assert_eq!(utm.source.as_deref(), Some("google"));
        assert_eq!(utm.medium.as_deref(), Some("cpc"));
        assert_eq!(utm.campaign.as_deref(), Some("winter"));
        assert_eq!(utm.content.as_deref(), Some("banner"));
        assert_eq!(utm.term.as_deref(), Some("analytics"));
    }

    #[test]
    fn test_utm_values_are_percent_decoded() {
        // Regression: `winter%20sale` and `winter+sale` used to be stored raw,
        // splitting one campaign across three different breakdown rows.
        let a = parse_utm_params("https://e.com/?utm_campaign=winter%20sale");
        let b = parse_utm_params("https://e.com/?utm_campaign=winter+sale");
        assert_eq!(a.campaign.as_deref(), Some("winter sale"));
        assert_eq!(b.campaign.as_deref(), Some("winter sale"));
        assert_eq!(a.campaign, b.campaign);
    }

    #[test]
    fn test_utm_decoding_handles_non_ascii() {
        let utm = parse_utm_params("https://e.com/?utm_campaign=caf%C3%A9");
        assert_eq!(utm.campaign.as_deref(), Some("café"));
    }

    #[test]
    fn test_utm_decoding_tolerates_malformed_escapes() {
        let utm = parse_utm_params("https://e.com/?utm_source=a%zz");
        assert_eq!(utm.source.as_deref(), Some("a%zz"));
    }

    #[test]
    fn test_utm_keys_are_case_insensitive() {
        let utm = parse_utm_params("https://e.com/?UTM_Source=Newsletter");
        assert_eq!(utm.source.as_deref(), Some("Newsletter"));
    }

    #[test]
    fn test_utm_ignores_fragment() {
        let utm = parse_utm_params("https://e.com/?utm_source=a#utm_source=b");
        assert_eq!(utm.source.as_deref(), Some("a"));
    }

    #[test]
    fn test_parse_utm_params_none() {
        assert_eq!(
            parse_utm_params("https://example.com/page"),
            UtmParams::default()
        );
    }

    #[test]
    fn test_parse_utm_partial() {
        let utm = parse_utm_params("https://example.com/?utm_source=google");
        assert_eq!(utm.source.as_deref(), Some("google"));
        assert!(utm.medium.is_none());
    }

    // ── Referrer classification ──────────────────────────────────────────

    #[test]
    fn test_referrer_sources() {
        let cases = [
            ("https://www.google.com/search?q=test", "Google"),
            ("https://t.co/abc", "Twitter"),
            ("https://www.facebook.com/", "Facebook"),
            ("https://www.reddit.com/r/rust", "Reddit"),
            ("https://github.com/user/repo", "GitHub"),
            ("https://www.linkedin.com/feed", "LinkedIn"),
            ("https://duckduckgo.com/?q=test", "DuckDuckGo"),
            ("https://news.ycombinator.com/item?id=1", "Hacker News"),
            ("https://x.com/someone", "Twitter"),
        ];
        for (url, expected) in cases {
            assert_eq!(
                extract_referrer_source(url).as_deref(),
                Some(expected),
                "for {url}"
            );
        }
    }

    #[test]
    fn test_referrer_matching_requires_a_domain_boundary() {
        // Regression: substring matching attributed these to Google/Facebook.
        assert_eq!(
            extract_referrer_source("https://notgoogle.com/x").as_deref(),
            Some("notgoogle.com")
        );
        assert_eq!(
            extract_referrer_source("https://google.com.phishing.example/x").as_deref(),
            Some("google.com.phishing.example")
        );
        assert_eq!(
            extract_referrer_source("https://myfacebook.io/").as_deref(),
            Some("myfacebook.io")
        );
    }

    #[test]
    fn test_referrer_subdomains_are_attributed() {
        assert_eq!(
            extract_referrer_source("https://mail.google.com/").as_deref(),
            Some("Google")
        );
    }

    #[test]
    fn test_referrer_unknown_host_is_normalised() {
        assert_eq!(
            extract_referrer_source("https://WWW.MyBlog.com/post").as_deref(),
            Some("myblog.com")
        );
    }

    #[test]
    fn test_referrer_strips_port_and_userinfo() {
        assert_eq!(
            extract_referrer_source("https://user:pw@myblog.com:8443/post").as_deref(),
            Some("myblog.com")
        );
    }

    #[test]
    fn test_extract_referrer_source_empty() {
        assert_eq!(extract_referrer_source(""), None);
    }

    // ── Device classification ────────────────────────────────────────────

    #[test]
    fn test_classify_device() {
        assert_eq!(classify_device(375), "mobile");
        assert_eq!(classify_device(768), "tablet");
        assert_eq!(classify_device(1920), "desktop");
    }

    // ── Pathname normalisation ───────────────────────────────────────────

    #[test]
    fn test_sanitize_pathname() {
        assert_eq!(
            sanitize_pathname("https://example.com/about?ref=1#section"),
            "/about"
        );
        assert_eq!(sanitize_pathname("https://example.com/"), "/");
        assert_eq!(
            sanitize_pathname("https://example.com/blog/post/123"),
            "/blog/post/123"
        );
    }

    #[test]
    fn test_sanitize_pathname_handles_bare_authority() {
        assert_eq!(sanitize_pathname("https://example.com"), "/");
    }

    #[test]
    fn test_sanitize_pathname_accepts_relative_paths() {
        // Regression: a relative URL lost its first segment, because the old
        // implementation always dropped one path component as the authority.
        assert_eq!(sanitize_pathname("/about"), "/about");
        assert_eq!(sanitize_pathname("/about/team"), "/about/team");
        assert_eq!(sanitize_pathname("about"), "/about");
    }

    #[test]
    fn test_sanitize_pathname_collapses_trailing_slash() {
        // `/about` and `/about/` are the same page; keeping both split its
        // traffic across two breakdown rows.
        assert_eq!(sanitize_pathname("https://e.com/about/"), "/about");
        assert_eq!(sanitize_pathname("/about/"), "/about");
        assert_eq!(sanitize_pathname("/"), "/", "the root keeps its slash");
    }

    // ── Field sanitisation ───────────────────────────────────────────────

    #[test]
    fn test_sanitize_string_truncate() {
        assert_eq!(sanitize_string(&"a".repeat(500), 256).len(), 256);
    }

    #[test]
    fn test_sanitize_string_control_chars() {
        assert_eq!(
            sanitize_string("hello\x00world\x01test", 256),
            "helloworldtest"
        );
    }

    #[test]
    fn test_props_must_be_a_json_object() {
        assert_eq!(
            sanitize_props(r#"{"plan":"pro"}"#).as_deref(),
            Some(r#"{"plan":"pro"}"#)
        );
        // Anything that is not an object would break every JSON query on the
        // column, so it is dropped rather than stored.
        assert!(sanitize_props("not json").is_none());
        assert!(sanitize_props("[1,2,3]").is_none());
        assert!(sanitize_props("42").is_none());
        assert!(sanitize_props("").is_none());
    }

    #[test]
    fn test_props_are_canonicalised() {
        assert_eq!(
            sanitize_props("  {\"a\" : 1}  ").as_deref(),
            Some(r#"{"a":1}"#)
        );
    }

    #[test]
    fn test_currency_normalisation() {
        assert_eq!(normalize_currency("usd").as_deref(), Some("USD"));
        assert_eq!(normalize_currency(" EUR ").as_deref(), Some("EUR"));
        // The old code truncated to three characters, so "DOLLARS" became "DOL".
        assert!(normalize_currency("DOLLARS").is_none());
        assert!(normalize_currency("US").is_none());
        assert!(normalize_currency("US1").is_none());
        assert!(normalize_currency("").is_none());
    }

    // ── Timestamps ───────────────────────────────────────────────────────

    #[test]
    fn test_round_to_hour() {
        let dt = chrono::DateTime::parse_from_rfc3339("2024-03-15T14:37:22Z")
            .unwrap()
            .with_timezone(&Utc);
        let rounded = round_to_hour(dt);
        assert_eq!(
            rounded.format("%Y-%m-%d %H:%M:%S").to_string(),
            "2024-03-15 14:00:00"
        );
    }

    #[test]
    fn test_round_to_hour_on_exact_hour_is_stable() {
        let dt = chrono::DateTime::parse_from_rfc3339("2024-03-15T14:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(round_to_hour(dt).format("%H:%M:%S").to_string(), "14:00:00");
    }

    #[test]
    fn test_strip_url_query_and_fragment() {
        assert_eq!(
            strip_url_query_and_fragment("https://google.com/search?q=cancer+diagnosis"),
            "https://google.com/search"
        );
        assert_eq!(
            strip_url_query_and_fragment("https://example.com/page#section"),
            "https://example.com/page"
        );
        assert_eq!(
            strip_url_query_and_fragment("https://example.com/page?a=1#s"),
            "https://example.com/page"
        );
        assert_eq!(
            strip_url_query_and_fragment("https://example.com/page"),
            "https://example.com/page"
        );
    }

    // ── build_event ──────────────────────────────────────────────────────

    fn payload(domain: &str) -> EventPayload {
        EventPayload {
            domain: domain.to_string(),
            name: "pageview".to_string(),
            url: "https://example.com/".to_string(),
            referrer: None,
            screen_width: Some(1920),
            props: None,
            revenue_amount: None,
            revenue_currency: None,
        }
    }

    #[test]
    fn test_build_event_populates_the_core_fields() {
        let state = crate::test_support::state_builder().build_state();
        let event = build_event(
            &state,
            &headers_with(&[("user-agent", "Mozilla/5.0 (Windows NT 10.0) Chrome/120.0")]),
            Some(addr("203.0.113.5:1234")),
            &payload("example.com"),
        )
        .unwrap();

        assert_eq!(event.site_id, "example.com");
        assert_eq!(event.event_name, "pageview");
        assert_eq!(event.pathname, "/");
        assert_eq!(event.browser.as_deref(), Some("Chrome"));
        assert_eq!(event.device_type.as_deref(), Some("desktop"));
        assert_eq!(event.visitor_id.len(), 64);
    }

    #[test]
    fn test_build_event_rejects_a_site_outside_the_allowlist() {
        // Regression: site_ids was only ever checked against the Origin header,
        // and a request without Origin skipped the check entirely — so anyone
        // could write events for any site_id.
        let state = crate::test_support::state_builder()
            .allowed_sites(vec!["allowed.com".to_string()])
            .build_state();
        let err = build_event(&state, &HeaderMap::new(), None, &payload("evil.com")).unwrap_err();
        assert_eq!(err, RejectReason::SiteNotAllowed);
        assert_eq!(err.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn test_build_event_accepts_an_allowlisted_site() {
        let state = crate::test_support::state_builder()
            .allowed_sites(vec!["allowed.com".to_string()])
            .build_state();
        assert!(build_event(&state, &HeaderMap::new(), None, &payload("allowed.com")).is_ok());
    }

    #[test]
    fn test_build_event_rejects_empty_fields() {
        let state = crate::test_support::state_builder().build_state();
        let mut p = payload("example.com");
        p.name = String::new();
        assert_eq!(
            build_event(&state, &HeaderMap::new(), None, &p).unwrap_err(),
            RejectReason::Invalid
        );
    }

    #[test]
    fn test_build_event_rejects_oversized_fields() {
        let state = crate::test_support::state_builder().build_state();
        let mut p = payload("example.com");
        p.url = "x".repeat(MAX_URL_LEN + 1);
        assert_eq!(
            build_event(&state, &HeaderMap::new(), None, &p).unwrap_err(),
            RejectReason::Invalid
        );
    }

    #[test]
    fn test_build_event_rejects_a_malformed_site_id() {
        let state = crate::test_support::state_builder().build_state();
        assert_eq!(
            build_event(&state, &HeaderMap::new(), None, &payload("has space.com")).unwrap_err(),
            RejectReason::SiteId
        );
    }

    #[test]
    fn test_build_event_filters_bots() {
        let state = crate::test_support::state_builder()
            .filter_bots(true)
            .build_state();
        let headers = headers_with(&[("user-agent", "Googlebot/2.1")]);
        let err = build_event(&state, &headers, None, &payload("example.com")).unwrap_err();
        assert_eq!(err, RejectReason::Bot);
        assert_eq!(
            err.status(),
            StatusCode::ACCEPTED,
            "bots are accepted-and-dropped so crawlers do not retry"
        );
    }

    #[test]
    fn test_build_event_applies_the_rate_limit() {
        let state = crate::test_support::state_builder()
            .rate_limit_per_site(1)
            .build_state();
        assert!(build_event(&state, &HeaderMap::new(), None, &payload("example.com")).is_ok());
        assert_eq!(
            build_event(&state, &HeaderMap::new(), None, &payload("example.com")).unwrap_err(),
            RejectReason::RateLimited
        );
    }

    #[test]
    fn test_build_event_applies_the_per_ip_rate_limit() {
        // A per-site budget alone lets one abusive client exhaust the whole
        // allowance and deny service to a site's real visitors.
        let state = crate::test_support::state_builder()
            .rate_limit_per_ip(1)
            .build_state();
        let peer = Some(addr("203.0.113.1:1000"));
        assert!(build_event(&state, &HeaderMap::new(), peer, &payload("a.com")).is_ok());
        assert_eq!(
            build_event(&state, &HeaderMap::new(), peer, &payload("b.com")).unwrap_err(),
            RejectReason::RateLimited,
            "the same client is limited across sites"
        );
    }

    #[test]
    fn test_visitor_ids_differ_per_site() {
        let state = crate::test_support::state_builder().build_state();
        let headers = headers_with(&[("user-agent", "UA")]);
        let peer = Some(addr("203.0.113.5:1234"));
        let a = build_event(&state, &headers, peer, &payload("a.com")).unwrap();
        let b = build_event(&state, &headers, peer, &payload("b.com")).unwrap();
        assert_ne!(
            a.visitor_id, b.visitor_id,
            "one person on two sites of an instance must not be correlatable"
        );
    }

    #[test]
    fn test_privacy_flags_apply_to_the_shared_path() {
        // The POST and pixel paths used to carry independent copies of this
        // logic, so a flag could apply to one and not the other.
        let state = crate::test_support::state_builder()
            .suppress_screen_size(true)
            .suppress_browser_version(true)
            .suppress_os_version(true)
            .round_timestamps(true)
            .build_state();
        let headers = headers_with(&[(
            "user-agent",
            "Mozilla/5.0 (Windows NT 10.0) Chrome/120.0.0.0",
        )]);

        let via_post = build_event(&state, &headers, None, &payload("example.com")).unwrap();
        let pixel_payload: EventPayload = PixelParams {
            domain: "example.com".to_string(),
            name: "pageview".to_string(),
            url: "https://example.com/".to_string(),
            referrer: None,
            screen_width: Some(1920),
        }
        .into();
        let via_pixel = build_event(&state, &headers, None, &pixel_payload).unwrap();

        for event in [&via_post, &via_pixel] {
            assert!(event.screen_size.is_none());
            assert!(event.device_type.is_none());
            assert!(event.browser_version.is_none());
            assert!(event.os_version.is_none());
            assert_eq!(event.timestamp.format("%M:%S").to_string(), "00:00");
        }
    }

    #[test]
    fn test_geoip_precision_none_drops_all_geography() {
        let state = crate::test_support::state_builder()
            .geoip_precision("none")
            .build_state();
        let event = build_event(&state, &HeaderMap::new(), None, &payload("example.com")).unwrap();
        assert!(event.country_code.is_none());
        assert!(event.region.is_none());
        assert!(event.city.is_none());
    }

    #[test]
    fn test_suppress_visitor_id_produces_a_random_identifier() {
        let state = crate::test_support::state_builder()
            .suppress_visitor_id(true)
            .build_state();
        let headers = headers_with(&[("user-agent", "UA")]);
        let peer = Some(addr("203.0.113.5:1234"));
        let a = build_event(&state, &headers, peer, &payload("a.com")).unwrap();
        let b = build_event(&state, &headers, peer, &payload("a.com")).unwrap();
        assert_ne!(a.visitor_id, b.visitor_id);
    }

    #[test]
    fn test_non_finite_revenue_is_dropped() {
        let state = crate::test_support::state_builder().build_state();
        let mut p = payload("example.com");
        p.revenue_amount = Some(f64::INFINITY);
        assert!(
            build_event(&state, &HeaderMap::new(), None, &p)
                .unwrap()
                .revenue_amount
                .is_none()
        );
    }

    #[test]
    fn test_strip_referrer_query_is_applied() {
        let state = crate::test_support::state_builder()
            .strip_referrer_query(true)
            .build_state();
        let mut p = payload("example.com");
        p.referrer = Some("https://google.com/search?q=private".to_string());
        let event = build_event(&state, &HeaderMap::new(), None, &p).unwrap();
        assert_eq!(event.referrer.as_deref(), Some("https://google.com/search"));
        assert_eq!(
            event.referrer_source.as_deref(),
            Some("Google"),
            "the source is derived before stripping"
        );
    }

    #[test]
    fn test_reject_reason_statuses() {
        assert_eq!(RejectReason::Origin.status(), StatusCode::FORBIDDEN);
        assert_eq!(RejectReason::SiteNotAllowed.status(), StatusCode::FORBIDDEN);
        assert_eq!(RejectReason::Invalid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(RejectReason::SiteId.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            RejectReason::RateLimited.status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(RejectReason::Bot.status(), StatusCode::ACCEPTED);
    }
}
