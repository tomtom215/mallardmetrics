//! Share links: revocable, read-only, public views of one site.
//!
//! The design and its threat model are written up in `docs/src/share-links.md`;
//! this header records only what the code must keep true.
//!
//! * A link is a bearer secret: `ms_` plus 256 bits from the operating system's
//!   CSPRNG, shown once at creation. Only its SHA-256 is stored, in
//!   `share_links.json` (owner-only, written atomically).
//! * The token travels in the `X-Share-Token` **header**, never in a URL the
//!   server sees. The dashboard reads it from the address bar's fragment
//!   (`/#share=<token>`), which browsers do not send to servers, put in a
//!   `Referer`, or hand to proxies.
//! * A share credential is accepted **only** by the routes in [`view_routes`],
//!   which are mounted outside the normal authentication layers. It is not an
//!   API key: `require_auth` and `require_admin_auth` do not know it exists, so
//!   a route that is not listed here cannot be reached with one by construction.
//! * [`share_gate`] runs before any handler. It forces `site_id` to the link's
//!   site (naming another site is refused), prepends the link's segment to
//!   `filters` (filters are AND-ed, so a viewer can only narrow), and rebuilds
//!   the query string from what it verified, so the handler sees exactly that.

use crate::api::auth::{anonymize_ip, constant_time_eq, hash_api_key};
use crate::api::errors::{ApiError, ApiJson};
use crate::api::persist::{JsonList, random_hex, storage_unwritable};
use crate::api::segments::{Segment, SegmentStore};
use crate::api::stats;
use crate::ingest::handler::{AppState, client_ip};
use crate::ingest::ratelimit::{RateLimiter, network_key};
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use chrono::{Duration as ChronoDuration, NaiveDateTime};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The request header that carries a share token.
pub const SHARE_TOKEN_HEADER: &str = "x-share-token";
/// Every share token starts with this, so a leaked one is recognisable to secret scanners.
pub const TOKEN_PREFIX: &str = "ms_";
/// Random bytes in a token: 256 bits, twice the 128-bit minimum.
const TOKEN_BYTES: usize = 32;
/// Length of a well-formed token.
const TOKEN_LEN: usize = TOKEN_PREFIX.len() + TOKEN_BYTES * 2;

/// Links that are neither revoked nor expired, per instance.
pub const MAX_ACTIVE_LINKS: usize = 100;
/// Records kept in `share_links.json`, revoked and expired ones included.
pub const MAX_STORED_LINKS: usize = 500;
/// How long a revoked or expired link stays listed before it is dropped.
const INACTIVE_RETENTION_DAYS: i64 = 30;
/// Longest a link may be made to live.
pub const MAX_EXPIRY_DAYS: u32 = 730;
/// Longest link label, in characters.
pub const MAX_LABEL_CHARS: usize = 128;

/// Requests per second one link may make (also its burst). A cold dashboard
/// load is about fifteen.
pub const PER_LINK_RATE: u32 = 30;
/// Requests per second all links together may make.
pub const GLOBAL_RATE: u32 = 120;
/// Refused credentials per second one client network may present.
pub const FAILURE_RATE: u32 = 5;
/// Distinct keys each limiter tracks.
const LIMITER_KEYS: usize = 10_000;
/// A link's `last_used_at` is written to disk at most this often.
const LAST_USED_FLUSH_EVERY: Duration = Duration::from_secs(600);

/// A share link as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareLink {
    /// Public identifier for admin operations. Not a credential.
    pub id: String,
    /// SHA-256 of the token. The token itself is never stored.
    pub token_hash: String,
    pub label: String,
    pub site_id: String,
    /// Name of the segment at creation, for display.
    pub segment_name: Option<String>,
    /// The segment's filters **as they were at creation**; empty for none.
    /// Later edits to the segment do not reach an existing link.
    pub filters: String,
    pub created_at: NaiveDateTime,
    pub expires_at: Option<NaiveDateTime>,
    pub last_used_at: Option<NaiveDateTime>,
    pub revoked: bool,
    pub revoked_at: Option<NaiveDateTime>,
}

impl ShareLink {
    /// Whether the link authorises requests at `now`.
    pub fn is_active(&self, now: NaiveDateTime) -> bool {
        !self.revoked && self.expires_at.is_none_or(|expires| now < expires)
    }

    fn status(&self, now: NaiveDateTime) -> &'static str {
        if self.revoked {
            "revoked"
        } else if self.is_active(now) {
            "active"
        } else {
            "expired"
        }
    }

    /// When the link stopped being usable, if it has.
    fn inactive_since(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        if self.revoked {
            self.revoked_at.or(self.expires_at)
        } else if self.is_active(now) {
            None
        } else {
            self.expires_at
        }
    }
}

/// What the gate needs to know about a link that authorised a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveLink {
    pub id: String,
    pub site_id: String,
    pub filters: String,
    pub segment_name: Option<String>,
    pub expires_at: Option<NaiveDateTime>,
}

/// Why a share-link operation failed.
#[derive(Debug)]
pub enum ShareError {
    Invalid(String),
    Conflict(String),
    NotFound,
    Io(std::io::Error),
    /// A revocation took effect in memory but could not be written.
    RevokedNotSaved(std::io::Error),
}

impl From<std::io::Error> for ShareError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// Validate a link label: 1 to [`MAX_LABEL_CHARS`] characters, no control characters.
fn validate_label(raw: &str) -> Result<String, ShareError> {
    let label = raw.trim();
    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
        return Err(ShareError::Invalid(format!(
            "Label must be 1-{MAX_LABEL_CHARS} characters"
        )));
    }
    if label.chars().any(char::is_control) {
        return Err(ShareError::Invalid(
            "Label must not contain control characters".to_string(),
        ));
    }
    Ok(label.to_string())
}

/// Hash a token for storage and lookup.
///
/// SHA-256 is enough because the input is 256 bits of randomness: there is
/// nothing to brute-force, so a slow password hash would only slow every
/// request.
pub fn hash_token(token: &str) -> String {
    hash_api_key(&format!("share-link/v1\0{token}"))
}

/// Whether `token` has the shape of one we issue. Checked before hashing so an
/// oversized or malformed header costs nothing.
fn well_formed(token: &str) -> bool {
    token.len() == TOKEN_LEN
        && token.starts_with(TOKEN_PREFIX)
        && token[TOKEN_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The share links.
#[derive(Clone)]
pub struct ShareLinkStore {
    list: JsonList<ShareLink>,
    /// Last-use times, kept apart from the list so recording a use neither
    /// races a concurrent edit nor writes the disk.
    touched: Arc<Mutex<HashMap<String, NaiveDateTime>>>,
    last_flush: Arc<Mutex<Instant>>,
}

impl Default for ShareLinkStore {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl ShareLinkStore {
    /// A store that is never written to disk.
    pub fn in_memory() -> Self {
        Self::with(JsonList::in_memory())
    }

    /// The store backed by `<data_dir>/share_links.json`.
    ///
    /// An unreadable file is moved aside and the store starts empty, which
    /// authorises nothing: it fails closed.
    pub fn load(data_dir: &FsPath) -> Self {
        let list = JsonList::load(data_dir.join("share_links.json"), "share_links.json");
        let store = Self::with(list);
        let count = store.list.read(<[ShareLink]>::len);
        if count > 0 {
            tracing::info!(count, "Loaded share links from disk");
        }
        store
    }

    fn with(list: JsonList<ShareLink>) -> Self {
        Self {
            list,
            touched: Arc::default(),
            last_flush: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Fold recorded uses into `links`.
    fn overlay(&self, links: &mut [ShareLink]) {
        let touched = self.touched.lock();
        for link in links {
            if let Some(used) = touched.get(&link.id)
                && link.last_used_at.is_none_or(|prior| *used > prior)
            {
                link.last_used_at = Some(*used);
            }
        }
    }

    /// Create a link. Blocks on file I/O. Returns the record and the token,
    /// which exists nowhere else.
    ///
    /// # Errors
    ///
    /// [`ShareError`].
    pub fn create(
        &self,
        label: &str,
        site_id: &str,
        segment: Option<&Segment>,
        expires_in_days: Option<u32>,
        now: NaiveDateTime,
    ) -> Result<(ShareLink, String), ShareError> {
        stats::validate_site_id(site_id).map_err(|e| ShareError::Invalid(e.to_string()))?;
        let label = validate_label(label)?;
        let expires_at = match expires_in_days {
            None => None,
            Some(days) if (1..=MAX_EXPIRY_DAYS).contains(&days) => {
                Some(now + ChronoDuration::days(i64::from(days)))
            }
            Some(_) => {
                return Err(ShareError::Invalid(format!(
                    "expires_in_days must be between 1 and {MAX_EXPIRY_DAYS}"
                )));
            }
        };
        if let Some(segment) = segment
            && segment.site_id != site_id
        {
            return Err(ShareError::Invalid(
                "That segment belongs to a different site".to_string(),
            ));
        }
        let token = format!("{TOKEN_PREFIX}{}", random_hex(TOKEN_BYTES)?);
        let record = ShareLink {
            id: random_hex(12)?,
            token_hash: hash_token(&token),
            label,
            site_id: site_id.to_string(),
            segment_name: segment.map(|s| s.name.clone()),
            filters: segment.map(|s| s.filters.clone()).unwrap_or_default(),
            created_at: now,
            expires_at,
            last_used_at: None,
            revoked: false,
            revoked_at: None,
        };
        self.list.update(|all| {
            self.overlay(all);
            prune(all, now);
            if all.iter().filter(|l| l.is_active(now)).count() >= MAX_ACTIVE_LINKS {
                return Err(ShareError::Conflict(format!(
                    "This instance already has {MAX_ACTIVE_LINKS} active share links; revoke one first"
                )));
            }
            // Active links are capped far below the stored cap, so an inactive
            // record always exists to make room.
            while all.len() >= MAX_STORED_LINKS {
                let oldest = all
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| !l.is_active(now))
                    .min_by_key(|(_, l)| l.created_at)
                    .map(|(i, _)| i);
                match oldest {
                    Some(i) => {
                        all.remove(i);
                    }
                    None => break,
                }
            }
            all.push(record.clone());
            Ok(())
        })?;
        Ok((record, token))
    }

    /// The link a token authorises at `now`, or `None` for an unknown,
    /// malformed, revoked or expired token.
    ///
    /// The token is hashed, and the hash compared with every stored hash in
    /// constant time without stopping at a match, so the time taken depends
    /// neither on which link matched nor on how many leading bytes agreed.
    pub fn resolve(&self, token: &str, now: NaiveDateTime) -> Option<ActiveLink> {
        if !well_formed(token) {
            return None;
        }
        let presented = hash_token(token);
        self.list.read(|links| {
            let mut found: Option<&ShareLink> = None;
            for link in links {
                let same = constant_time_eq(link.token_hash.as_bytes(), presented.as_bytes());
                if same && found.is_none() {
                    found = Some(link);
                }
            }
            found
                .filter(|link| link.is_active(now))
                .map(|link| ActiveLink {
                    id: link.id.clone(),
                    site_id: link.site_id.clone(),
                    filters: link.filters.clone(),
                    segment_name: link.segment_name.clone(),
                    expires_at: link.expires_at,
                })
        })
    }

    /// Record a use. Returns whether the use times are due to be written.
    pub fn touch(&self, id: &str, now: NaiveDateTime) -> bool {
        self.touched.lock().insert(id.to_string(), now);
        let mut last = self.last_flush.lock();
        if last.elapsed() >= LAST_USED_FLUSH_EVERY {
            *last = Instant::now();
            true
        } else {
            false
        }
    }

    /// Write recorded use times to disk. Blocks on file I/O.
    ///
    /// # Errors
    ///
    /// The I/O error from writing the file.
    pub fn flush_use_times(&self) -> std::io::Result<()> {
        self.list.update(|all| {
            self.overlay(all);
            Ok::<_, std::io::Error>(())
        })
    }

    /// Revoke a link. Idempotent. Blocks on file I/O.
    ///
    /// The revocation takes effect in memory **before** the write, and stays
    /// in effect if the write fails: a kill switch that does nothing when the
    /// disk is full is the wrong way round. The caller is told, because after
    /// a restart the link could come back.
    ///
    /// # Errors
    ///
    /// [`ShareError::NotFound`], or [`ShareError::RevokedNotSaved`].
    pub fn revoke(&self, id: &str, now: NaiveDateTime) -> Result<(), ShareError> {
        let found = self.list.update_in_memory(|all| {
            let Some(link) = all.iter_mut().find(|l| l.id == id) else {
                return false;
            };
            if !link.revoked {
                link.revoked = true;
                link.revoked_at = Some(now);
            }
            true
        });
        if !found {
            return Err(ShareError::NotFound);
        }
        self.list
            .update(|all| {
                self.overlay(all);
                Ok::<_, std::io::Error>(())
            })
            .map_err(ShareError::RevokedNotSaved)
    }

    /// Every link, newest first, with use times folded in. Never the token or its hash.
    pub fn list(&self) -> Vec<ShareLink> {
        let mut links = self.list.snapshot();
        self.overlay(&mut links);
        links.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        links
    }

    /// Test hook: set a link's expiry.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_expiry_for_test(&self, id: &str, expires_at: Option<NaiveDateTime>) {
        self.list.update_in_memory(|all| {
            if let Some(link) = all.iter_mut().find(|l| l.id == id) {
                link.expires_at = expires_at;
            }
        });
    }
}

/// Drop links that stopped being usable more than the retention period ago.
fn prune(all: &mut Vec<ShareLink>, now: NaiveDateTime) {
    let keep_after = now - ChronoDuration::days(INACTIVE_RETENTION_DAYS);
    all.retain(|l| l.inactive_since(now).is_none_or(|since| since > keep_after));
}

/// Everything the share feature keeps in `AppState`.
#[derive(Clone)]
pub struct ShareState {
    pub links: ShareLinkStore,
    pub segments: SegmentStore,
    /// Valid-token requests, keyed by link.
    link_limiter: RateLimiter,
    /// Valid-token requests, all links.
    global_limiter: RateLimiter,
    /// Refused credentials, keyed by client network. Kept apart from the two
    /// above so someone guessing tokens can spend only their own budget, not
    /// the viewers'.
    failure_limiter: RateLimiter,
}

impl Default for ShareState {
    fn default() -> Self {
        Self::in_memory()
    }
}

impl ShareState {
    /// State that is never written to disk.
    pub fn in_memory() -> Self {
        Self::with(ShareLinkStore::in_memory(), SegmentStore::in_memory())
    }

    /// State backed by `share_links.json` and `segments.json` in `data_dir`.
    pub fn load(data_dir: &FsPath) -> Self {
        Self::with(ShareLinkStore::load(data_dir), SegmentStore::load(data_dir))
    }

    fn with(links: ShareLinkStore, segments: SegmentStore) -> Self {
        Self {
            links,
            segments,
            link_limiter: RateLimiter::new(PER_LINK_RATE, LIMITER_KEYS),
            global_limiter: RateLimiter::new(GLOBAL_RATE, 1),
            failure_limiter: RateLimiter::new(FAILURE_RATE, LIMITER_KEYS),
        }
    }

    /// Replace the limiters, for tests that need a tiny budget.
    #[cfg(any(test, feature = "testing"))]
    #[must_use]
    pub fn with_limits(mut self, per_link: u32, global: u32, failures: u32) -> Self {
        self.link_limiter = RateLimiter::new(per_link, LIMITER_KEYS);
        self.global_limiter = RateLimiter::new(global, 1);
        self.failure_limiter = RateLimiter::new(failures, LIMITER_KEYS);
        self
    }
}

// ── Query scoping ────────────────────────────────────────────────────────

/// Why a viewer's query was not accepted.
#[derive(Debug, PartialEq, Eq)]
pub enum ScopeError {
    /// The query string could not be parsed, or repeats `site_id` or `filters`.
    Malformed(String),
    /// `site_id` names a site other than the link's.
    OtherSite,
}

fn encode_component(raw: &str, out: &mut String) {
    use std::fmt::Write;
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
}

/// Build the query string a handler will see for a viewer's request.
///
/// * `site_id` must be absent or exactly the link's; absent is filled in.
/// * `site_id` and `filters` may each appear once. A repeated parameter is a
///   request whose meaning depends on which copy a parser keeps, so it is
///   refused instead of guessed at.
/// * `filters` becomes the link's segment followed by the viewer's own. The
///   parser ANDs every condition, so the viewer can narrow the segment but
///   never remove or widen it.
/// * The result is encoded from the parsed values, so the handler's parser sees
///   exactly what was checked here.
///
/// # Errors
///
/// [`ScopeError`].
pub fn scope_query(link_site: &str, link_filters: &str, uri: &Uri) -> Result<String, ScopeError> {
    let pairs = if uri.query().is_some() {
        axum::extract::Query::<Vec<(String, String)>>::try_from_uri(uri)
            .map_err(|_| ScopeError::Malformed("The query string is not valid".to_string()))?
            .0
    } else {
        Vec::new()
    };

    let mut viewer_filters: Option<String> = None;
    let mut site_seen = false;
    let mut others: Vec<(String, String)> = Vec::new();
    for (name, value) in pairs {
        match name.as_str() {
            "site_id" => {
                if site_seen {
                    return Err(ScopeError::Malformed(
                        "site_id must be given at most once".to_string(),
                    ));
                }
                site_seen = true;
                if value != link_site {
                    return Err(ScopeError::OtherSite);
                }
            }
            "filters" => {
                if viewer_filters.is_some() {
                    return Err(ScopeError::Malformed(
                        "filters must be given at most once".to_string(),
                    ));
                }
                viewer_filters = Some(value);
            }
            _ => others.push((name, value)),
        }
    }

    let viewer = viewer_filters.unwrap_or_default();
    let viewer = viewer.trim();
    let combined = match (link_filters.is_empty(), viewer.is_empty()) {
        (true, true) => String::new(),
        (true, false) => viewer.to_string(),
        (false, true) => link_filters.to_string(),
        (false, false) => format!("{link_filters};{viewer}"),
    };

    let mut out = String::from("site_id=");
    encode_component(link_site, &mut out);
    if !combined.is_empty() {
        out.push_str("&filters=");
        encode_component(&combined, &mut out);
    }
    for (name, value) in others {
        out.push('&');
        encode_component(&name, &mut out);
        out.push('=');
        encode_component(&value, &mut out);
    }
    Ok(out)
}

// ── The gate ─────────────────────────────────────────────────────────────

fn refusal(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "code": code, "error": message })),
    )
        .into_response()
}

/// One answer for an unknown, malformed, expired or revoked token, so the
/// response does not say which.
fn invalid_link() -> Response {
    refusal(
        StatusCode::UNAUTHORIZED,
        "share_link_invalid",
        "This share link is invalid, has expired, or was revoked.",
    )
}

fn busy(message: &str) -> Response {
    let mut response = ApiError::TooManyRequests(message.to_string()).into_response();
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    response
}

/// Authorise a share request and confine it to its link.
///
/// Runs on every route in [`view_routes`], before the handler.
pub async fn share_gate(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let (mut parts, body) = request.into_parts();
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr);
    let ip = client_ip(&state, &parts.headers, peer);
    let now = chrono::Utc::now().naive_utc();

    let link = parts
        .headers
        .get(SHARE_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|token| state.share.links.resolve(token, now));
    let Some(link) = link else {
        // Only refused credentials spend the failure budget, and it is the
        // client's own: guessing cannot use up what viewers need.
        if !state.share.failure_limiter.check(&network_key(&ip)) {
            return busy("Too many refused share credentials from this address.");
        }
        tracing::debug!(
            ip_prefix = %anonymize_ip(&ip),
            "Share request refused: no valid credential"
        );
        return invalid_link();
    };

    if !state.share.link_limiter.check(&link.id) {
        return busy("This share link is being used too heavily. Try again in a moment.");
    }
    if !state.share.global_limiter.check("all") {
        return busy("Shared views are busy. Try again in a moment.");
    }

    match scope_query(&link.site_id, &link.filters, &parts.uri) {
        Ok(query) => {
            let path = parts.uri.path();
            match format!("{path}?{query}").parse::<Uri>() {
                Ok(uri) => parts.uri = uri,
                Err(_) => {
                    return refusal(
                        StatusCode::BAD_REQUEST,
                        "bad_request",
                        "The request could not be understood",
                    );
                }
            }
        }
        Err(ScopeError::OtherSite) => {
            // Debug, not warn: the holder of a link controls how often this fires
            // (up to the per-link limit, every second), and a warn per request
            // would let them fill the log. The refusal itself is the 403.
            tracing::debug!(link_id = %link.id, "Share link used to ask for a different site; refused");
            return refusal(
                StatusCode::FORBIDDEN,
                "share_site_forbidden",
                "This share link only shows its own site.",
            );
        }
        Err(ScopeError::Malformed(message)) => {
            return refusal(StatusCode::BAD_REQUEST, "bad_request", &message);
        }
    }

    if state.share.links.touch(&link.id, now) {
        let links = state.share.links.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(e) = links.flush_use_times() {
                tracing::warn!(error = %e, "Could not write share link use times");
            }
        });
    }

    parts.extensions.insert(link);
    // A share link's segment is a confidentiality boundary: run the request so
    // that every report cuts events to the segment instead of measuring whole
    // visits that merely touch it (see `query::SEGMENT_CUTS_EVENTS`).
    crate::query::SEGMENT_CUTS_EVENTS
        .scope(true, next.run(Request::from_parts(parts, body)))
        .await
}

/// Headers every share response carries, errors included.
///
/// `no-referrer`: whatever the viewer's page links to learns nothing about where
/// it was opened. `no-store`: no cache, shared or private, keeps another
/// person's view of the data once the link is revoked.
async fn share_response_headers(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-robots-tag",
        HeaderValue::from_static("noindex, nofollow"),
    );
    response
}

/// GET /api/shared/session — what a viewer's dashboard needs to know about its link.
async fn share_session(Extension(link): Extension<ActiveLink>) -> Response {
    Json(serde_json::json!({
        "site_id": link.site_id,
        "segment": link.segment_name,
        "expires_at": link.expires_at.map(format_time),
    }))
    .into_response()
}

/// The routes a share credential opens.
///
/// Read-only stats for one site, and nothing else. Add a route here only after asking what it would let a
/// stranger do; exports, the site list, keys, erasure and backup are absent on
/// purpose, and `tests/share_test.rs` pins that.
pub fn view_routes(state: Arc<AppState>) -> Router<Arc<AppState>> {
    Router::new()
        .route("/shared/session", get(share_session))
        .route("/shared/stats/main", get(stats::get_main_stats))
        .route("/shared/stats/timeseries", get(stats::get_timeseries))
        .route("/shared/stats/sessions", get(stats::get_sessions))
        .route("/shared/stats/realtime", get(stats::get_realtime))
        .route("/shared/stats/revenue", get(stats::get_revenue))
        .route("/shared/stats/goals", get(stats::get_goals))
        .route("/shared/stats/properties", get(stats::get_property_keys))
        .route(
            "/shared/stats/property-values",
            get(stats::get_property_values),
        )
        .route(
            "/shared/stats/breakdown/{dimension}",
            get(stats::get_breakdown),
        )
        .route("/shared/stats/funnel", get(stats::get_funnel))
        .route("/shared/stats/retention", get(stats::get_retention))
        .route("/shared/stats/sequences", get(stats::get_sequences))
        .route("/shared/stats/flow", get(stats::get_flow))
        .layer(middleware::from_fn_with_state(state, share_gate))
        .layer(middleware::map_response(share_response_headers))
}

// ── Admin endpoints ──────────────────────────────────────────────────────

fn format_time(t: NaiveDateTime) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn view(link: &ShareLink, now: NaiveDateTime) -> serde_json::Value {
    serde_json::json!({
        "id": link.id,
        "label": link.label,
        "site_id": link.site_id,
        "segment": link.segment_name,
        "filters": link.filters,
        "created_at": format_time(link.created_at),
        "expires_at": link.expires_at.map(format_time),
        "last_used_at": link.last_used_at.map(format_time),
        "status": link.status(now),
    })
}

fn respond(error: ShareError, action: &str) -> Response {
    match error {
        ShareError::Invalid(m) => ApiError::BadRequest(m).into_response(),
        ShareError::Conflict(m) => ApiError::Conflict(m).into_response(),
        ShareError::NotFound => {
            ApiError::NotFound("Share link not found".to_string()).into_response()
        }
        ShareError::Io(e) => storage_unwritable(action, &e),
        ShareError::RevokedNotSaved(e) => {
            tracing::error!(error = %e, "Share link revoked in memory but not saved");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "code": "internal_error",
                    "error": "The link is revoked now, but the change could not be saved, so it \
                              may work again after a restart. Check that the data directory is \
                              writable and revoke it again."
                })),
            )
                .into_response()
        }
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ShareError> + Send + 'static,
) -> Result<T, ShareError> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| {
        tracing::error!(error = %e, "Share link store task failed");
        Err(ShareError::Io(std::io::Error::other("task failed")))
    })
}

/// Body of `POST /api/share-links`.
#[derive(Debug, Deserialize)]
pub struct CreateShareLink {
    pub label: String,
    pub site_id: String,
    /// Restrict the link to this saved segment. Its filters are copied now.
    pub segment_id: Option<String>,
    /// Days until the link stops working. Omit for no expiry.
    pub expires_in_days: Option<u32>,
}

/// POST /api/share-links — create a link (admin). The token is in this response only.
pub async fn create_share_link(
    State(state): State<Arc<AppState>>,
    ApiJson(body): ApiJson<CreateShareLink>,
) -> Response {
    let segment = match &body.segment_id {
        None => None,
        Some(id) => match state.share.segments.get(id) {
            Some(segment) => Some(segment),
            None => return ApiError::NotFound("Segment not found".to_string()).into_response(),
        },
    };
    let links = state.share.links.clone();
    let now = chrono::Utc::now().naive_utc();
    let result = blocking(move || {
        links.create(
            &body.label,
            &body.site_id,
            segment.as_ref(),
            body.expires_in_days,
            now,
        )
    })
    .await;
    match result {
        Ok((link, token)) => {
            // The id identifies the link in the log; the token never appears.
            tracing::info!(link_id = %link.id, site_id = %link.site_id, "Share link created");
            let mut created = view(&link, now);
            created["token"] = serde_json::Value::String(token);
            (StatusCode::CREATED, Json(created)).into_response()
        }
        Err(e) => respond(e, "create the share link"),
    }
}

/// GET /api/share-links — list links (admin). Never the token or its hash.
pub async fn list_share_links(State(state): State<Arc<AppState>>) -> Response {
    let now = chrono::Utc::now().naive_utc();
    let body: Vec<_> = state
        .share
        .links
        .list()
        .iter()
        .map(|l| view(l, now))
        .collect();
    Json(body).into_response()
}

/// DELETE /api/share-links/{id} — revoke a link (admin).
pub async fn revoke_share_link(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let links = state.share.links.clone();
    let now = chrono::Utc::now().naive_utc();
    let target = id.clone();
    match blocking(move || links.revoke(&target, now)).await {
        Ok(()) => {
            tracing::info!(link_id = %id, "Share link revoked");
            Json(serde_json::json!({"status": "revoked"})).into_response()
        }
        Err(e) => respond(e, "revoke the share link"),
    }
}

// ── Log redaction ────────────────────────────────────────────────────────

/// Replace anything shaped like a share token in `text` with `ms_[redacted]`.
///
/// The token is meant to travel in a header, which the request log never
/// records. This is the backstop for the day someone pastes one into a URL
/// anyway: the request span records the URI of every request, and a token in
/// it would be written to the log with every line logged under that span.
pub fn redact_tokens(text: &str) -> std::borrow::Cow<'_, str> {
    let prefix = TOKEN_PREFIX.as_bytes();
    let has_prefix = |s: &str| {
        s.as_bytes()
            .windows(prefix.len())
            .any(|w| w.eq_ignore_ascii_case(prefix))
    };
    // Percent-escapes are decoded and the prefix is matched without regard to
    // case, so `?x=MS_...` and `/%6d%73_...` are caught as well as `ms_...`.
    // A URI that holds a token is therefore logged in decoded form; one that
    // holds none is returned untouched.
    let decoded = percent_decode(text);
    if !has_prefix(&decoded) {
        return std::borrow::Cow::Borrowed(text);
    }
    let bytes = decoded.as_bytes();
    let mut out = String::with_capacity(decoded.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes.len() - i >= prefix.len()
            && bytes[i..i + prefix.len()].eq_ignore_ascii_case(prefix)
        {
            let start = i + prefix.len();
            let run = bytes[start..]
                .iter()
                .take_while(|b| b.is_ascii_hexdigit())
                .count();
            // Eight hex digits cannot be a coincidence in a path or query, and
            // any run that long may be the front of a truncated token.
            if run >= 8 {
                out.push_str(TOKEN_PREFIX);
                out.push_str("[redacted]");
                i = start + run;
                continue;
            }
        }
        // Copy one whole character; `i` is always on a boundary.
        let ch = decoded[i..].chars().next().unwrap_or('\u{fffd}');
        out.push(ch);
        i += ch.len_utf8();
    }
    std::borrow::Cow::Owned(out)
}

/// `text` with `%XX` escapes decoded (invalid UTF-8 replaced), or `text` itself
/// when it has none. Only used to look for tokens, never to interpret a URI.
fn percent_decode(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains('%') {
        return std::borrow::Cow::Borrowed(text);
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
        {
            out.push(u8::try_from(h * 16 + l).unwrap_or(b'?'));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    std::borrow::Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

/// The request span, as `tower_http`'s default makes it but with the URI redacted.
pub fn request_span<B>(request: &axum::http::Request<B>) -> tracing::Span {
    tracing::debug_span!(
        "request",
        method = %request.method(),
        uri = %redact_tokens(&request.uri().to_string()),
        version = ?request.version(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> NaiveDateTime {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    fn uri(q: &str) -> Uri {
        format!("/api/shared/stats/main?{q}").parse().unwrap()
    }

    fn store() -> ShareLinkStore {
        ShareLinkStore::in_memory()
    }

    fn segment(site: &str, filters: &str) -> Segment {
        SegmentStore::in_memory()
            .create(site, "seg", filters)
            .unwrap()
    }

    // ── tokens ──

    #[test]
    fn test_token_has_256_bits_and_a_recognisable_prefix() {
        let (_, token) = store().create("l", "a.com", None, None, now()).unwrap();
        assert!(token.starts_with("ms_"));
        let hex_part = &token[3..];
        assert_eq!(hex_part.len(), 64, "32 bytes = 256 bits, hex encoded");
        assert!(hex_part.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn test_tokens_are_distinct() {
        let s = store();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..50 {
            let (_, token) = s.create("l", "a.com", None, None, now()).unwrap();
            assert!(seen.insert(token), "a token repeated");
        }
    }

    #[test]
    fn test_only_the_hash_is_stored_and_listed() {
        let s = store();
        let (link, token) = s.create("l", "a.com", None, None, now()).unwrap();
        assert_ne!(link.token_hash, token);
        assert_eq!(link.token_hash, hash_token(&token));
        let listed = serde_json::to_string(&view(&s.list()[0], now())).unwrap();
        assert!(!listed.contains(&token));
        assert!(!listed.contains(&link.token_hash));
        assert!(!listed.contains("token"));
    }

    #[test]
    fn test_resolve_accepts_only_the_exact_token() {
        let s = store();
        let (link, token) = s.create("l", "a.com", None, None, now()).unwrap();
        assert_eq!(s.resolve(&token, now()).unwrap().id, link.id);
        let mut wrong = token.clone();
        wrong.replace_range(10..11, if &token[10..11] == "0" { "1" } else { "0" });
        assert!(s.resolve(&wrong, now()).is_none());
        for bad in [
            "",
            "ms_",
            "ms_zz",
            &token[..66],
            &format!("{token}0"),
            &token.to_uppercase(),
        ] {
            assert!(s.resolve(bad, now()).is_none(), "{bad:?}");
        }
        // An API key is not a share token.
        assert!(
            s.resolve(&crate::api::auth::generate_api_key(), now())
                .is_none()
        );
    }

    // ── expiry and revocation ──

    #[test]
    fn test_expiry_takes_effect_at_the_instant() {
        let s = store();
        let (_, token) = s.create("l", "a.com", None, Some(1), now()).unwrap();
        let expiry = now() + ChronoDuration::days(1);
        assert!(
            s.resolve(&token, expiry - ChronoDuration::seconds(1))
                .is_some()
        );
        assert!(
            s.resolve(&token, expiry).is_none(),
            "expired at the instant"
        );
        assert!(
            s.resolve(&token, expiry + ChronoDuration::days(9))
                .is_none()
        );
    }

    #[test]
    fn test_no_expiry_never_expires() {
        let s = store();
        let (_, token) = s.create("l", "a.com", None, None, now()).unwrap();
        assert!(
            s.resolve(&token, now() + ChronoDuration::days(36_500))
                .is_some()
        );
    }

    #[test]
    fn test_expiry_bounds() {
        let s = store();
        for bad in [0, MAX_EXPIRY_DAYS + 1, u32::MAX] {
            assert!(matches!(
                s.create("l", "a.com", None, Some(bad), now()),
                Err(ShareError::Invalid(_))
            ));
        }
        assert!(
            s.create("l", "a.com", None, Some(MAX_EXPIRY_DAYS), now())
                .is_ok()
        );
    }

    #[test]
    fn test_revocation_takes_effect_on_the_next_lookup() {
        let s = store();
        let (link, token) = s.create("l", "a.com", None, None, now()).unwrap();
        assert!(s.resolve(&token, now()).is_some());
        s.revoke(&link.id, now()).unwrap();
        assert!(s.resolve(&token, now()).is_none());
        s.revoke(&link.id, now()).unwrap();
        assert!(matches!(s.revoke("nope", now()), Err(ShareError::NotFound)));
        assert_eq!(s.list()[0].status(now()), "revoked");
    }

    #[test]
    fn test_revocation_holds_even_when_the_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let s = ShareLinkStore::load(dir.path());
        let (link, token) = s.create("l", "a.com", None, None, now()).unwrap();
        // Replace the file's directory entry so the next write cannot happen.
        std::fs::remove_file(dir.path().join("share_links.json")).unwrap();
        std::fs::create_dir(dir.path().join("share_links.json")).unwrap();
        let result = s.revoke(&link.id, now());
        assert!(matches!(result, Err(ShareError::RevokedNotSaved(_))));
        assert!(
            s.resolve(&token, now()).is_none(),
            "revoked despite the failed write"
        );
    }

    // ── caps ──

    #[test]
    fn test_active_link_cap() {
        let s = store();
        for i in 0..MAX_ACTIVE_LINKS {
            s.create(&format!("l{i}"), "a.com", None, None, now())
                .unwrap();
        }
        assert!(matches!(
            s.create("one more", "a.com", None, None, now()),
            Err(ShareError::Conflict(m)) if m.contains("100")
        ));
    }

    #[test]
    fn test_revoking_frees_a_slot() {
        let s = store();
        let mut first = None;
        for i in 0..MAX_ACTIVE_LINKS {
            let (link, _) = s
                .create(&format!("l{i}"), "a.com", None, None, now())
                .unwrap();
            first.get_or_insert(link.id);
        }
        s.revoke(&first.unwrap(), now()).unwrap();
        assert!(s.create("fits now", "a.com", None, None, now()).is_ok());
    }

    #[test]
    fn test_expired_links_do_not_count_against_the_cap() {
        let s = store();
        for i in 0..MAX_ACTIVE_LINKS {
            s.create(&format!("l{i}"), "a.com", None, Some(1), now())
                .unwrap();
        }
        let later = now() + ChronoDuration::days(2);
        assert!(s.create("fresh", "a.com", None, None, later).is_ok());
    }

    #[test]
    fn test_inactive_links_are_pruned_after_retention() {
        let s = store();
        let (link, _) = s.create("old", "a.com", None, None, now()).unwrap();
        s.revoke(&link.id, now()).unwrap();
        let much_later = now() + ChronoDuration::days(INACTIVE_RETENTION_DAYS + 1);
        s.create("new", "a.com", None, None, much_later).unwrap();
        assert!(s.list().iter().all(|l| l.label != "old"));
    }

    #[test]
    fn test_stored_record_cap_evicts_the_oldest_inactive() {
        let s = store();
        // Fill the file with revoked records inside the retention window.
        for i in 0..MAX_STORED_LINKS {
            let (link, _) = s
                .create(&format!("r{i}"), "a.com", None, None, now())
                .unwrap();
            s.revoke(&link.id, now()).unwrap();
        }
        assert_eq!(s.list().len(), MAX_STORED_LINKS);
        s.create("live", "a.com", None, None, now()).unwrap();
        assert_eq!(s.list().len(), MAX_STORED_LINKS, "bounded");
        assert!(s.list().iter().any(|l| l.label == "live"));
    }

    // ── segment snapshot ──

    #[test]
    fn test_segment_is_copied_by_value() {
        let segments = SegmentStore::in_memory();
        let seg = segments.create("a.com", "DE", "countries==DE").unwrap();
        let s = store();
        let (link, token) = s.create("l", "a.com", Some(&seg), None, now()).unwrap();
        assert_eq!(link.filters, "countries==DE");
        assert_eq!(link.segment_name.as_deref(), Some("DE"));
        // Editing or deleting the segment afterwards changes nothing.
        segments
            .replace(&seg.id, None, Some("countries==DE;countries==FR"))
            .unwrap();
        segments.delete(&seg.id).unwrap();
        assert_eq!(s.resolve(&token, now()).unwrap().filters, "countries==DE");
    }

    #[test]
    fn test_segment_of_another_site_is_refused() {
        let seg = segment("other.com", "countries==DE");
        assert!(matches!(
            store().create("l", "a.com", Some(&seg), None, now()),
            Err(ShareError::Invalid(_))
        ));
    }

    #[test]
    fn test_label_and_site_validation() {
        let s = store();
        for bad in ["", "  ", "a\nb", &"x".repeat(MAX_LABEL_CHARS + 1)] {
            assert!(
                matches!(
                    s.create(bad, "a.com", None, None, now()),
                    Err(ShareError::Invalid(_))
                ),
                "{bad:?}"
            );
        }
        assert!(matches!(
            s.create("l", "../x", None, None, now()),
            Err(ShareError::Invalid(_))
        ));
    }

    // ── persistence ──

    #[test]
    fn test_hashes_only_on_disk_and_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let s = ShareLinkStore::load(dir.path());
        let (link, token) = s.create("l", "a.com", None, None, now()).unwrap();
        let path = dir.path().join("share_links.json");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains(&token),
            "the token must never reach the disk"
        );
        assert!(!text.contains(&token[3..]), "nor its hex body");
        assert!(text.contains(&link.token_hash));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn test_links_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let (link, token) = ShareLinkStore::load(dir.path())
            .create("l", "a.com", None, Some(3), now())
            .unwrap();
        let again = ShareLinkStore::load(dir.path());
        assert_eq!(again.resolve(&token, now()).unwrap().id, link.id);
    }

    #[test]
    fn test_revocation_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let s = ShareLinkStore::load(dir.path());
        let (link, token) = s.create("l", "a.com", None, None, now()).unwrap();
        s.revoke(&link.id, now()).unwrap();
        assert!(
            ShareLinkStore::load(dir.path())
                .resolve(&token, now())
                .is_none()
        );
    }

    #[test]
    fn test_corrupt_store_fails_closed_and_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let s = ShareLinkStore::load(dir.path());
        let (_, token) = s.create("l", "a.com", None, None, now()).unwrap();
        std::fs::write(dir.path().join("share_links.json"), b"][ garbage").unwrap();
        let again = ShareLinkStore::load(dir.path());
        assert!(
            again.resolve(&token, now()).is_none(),
            "an unreadable store authorises nothing"
        );
        assert!(again.list().is_empty());
        let kept = std::fs::read_dir(dir.path()).unwrap().flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("share_links.json.corrupt-")
        });
        assert!(kept, "the unreadable file is set aside, not overwritten");
        // And the store is usable afterwards.
        assert!(again.create("l2", "a.com", None, None, now()).is_ok());
    }

    #[test]
    fn test_stale_temp_files_are_cleaned_at_start() {
        let dir = tempfile::tempdir().unwrap();
        let stale = dir.path().join("share_links.tmp.123.4");
        std::fs::write(&stale, b"x").unwrap();
        let _ = ShareLinkStore::load(dir.path());
        assert!(!stale.exists());
    }

    #[test]
    fn test_failed_create_is_not_live() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let s = ShareLinkStore::load(&blocker);
        assert!(matches!(
            s.create("l", "a.com", None, None, now()),
            Err(ShareError::Io(_))
        ));
        assert!(s.list().is_empty());
    }

    #[test]
    fn test_last_use_is_recorded_and_persisted_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let s = ShareLinkStore::load(dir.path());
        let (link, _) = s.create("l", "a.com", None, None, now()).unwrap();
        assert!(s.list()[0].last_used_at.is_none());
        let used = now() + ChronoDuration::minutes(5);
        s.touch(&link.id, used);
        assert_eq!(s.list()[0].last_used_at, Some(used));
        s.flush_use_times().unwrap();
        assert_eq!(
            ShareLinkStore::load(dir.path()).list()[0].last_used_at,
            Some(used)
        );
    }

    // ── query scoping ──

    #[test]
    fn test_site_is_forced_and_filled_in() {
        assert_eq!(
            scope_query("a.com", "", &uri("period=7d")).unwrap(),
            "site_id=a.com&period=7d"
        );
        assert_eq!(
            scope_query("a.com", "", &uri("site_id=a.com&period=7d")).unwrap(),
            "site_id=a.com&period=7d"
        );
        assert_eq!(
            scope_query("a.com", "", &"/x".parse().unwrap()).unwrap(),
            "site_id=a.com"
        );
    }

    #[test]
    fn test_another_site_is_refused_not_rewritten() {
        for q in [
            "site_id=b.com",
            "site_id=A.com",
            "site_id=a.com%20",
            "site_id=",
            "site_id=a.com.evil.com",
            "site%5Fid=b.com",
        ] {
            assert_eq!(
                scope_query("a.com", "", &uri(q)),
                Err(ScopeError::OtherSite),
                "{q}"
            );
        }
    }

    #[test]
    fn test_repeated_site_or_filters_are_refused() {
        assert!(matches!(
            scope_query("a.com", "", &uri("site_id=a.com&site_id=a.com")),
            Err(ScopeError::Malformed(_))
        ));
        assert!(matches!(
            scope_query("a.com", "countries==DE", &uri("filters=a==b&filters=c==d")),
            Err(ScopeError::Malformed(_))
        ));
        // Even when the first copy is the link's own and the second is foreign.
        assert!(scope_query("a.com", "", &uri("site_id=a.com&site_id=b.com")).is_err());
    }

    #[test]
    fn test_segment_is_prepended_and_cannot_be_removed() {
        let seg = "countries==DE";
        assert_eq!(
            scope_query("a.com", seg, &uri("period=7d")).unwrap(),
            "site_id=a.com&filters=countries%3D%3DDE&period=7d"
        );
        // The viewer's own filters follow the segment.
        let q = scope_query("a.com", seg, &uri("filters=browsers%3D%3DChrome")).unwrap();
        assert_eq!(
            q,
            "site_id=a.com&filters=countries%3D%3DDE%3Bbrowsers%3D%3DChrome"
        );
        // An empty or blank viewer value cannot clear it.
        for blank in ["filters=", "filters=%20%20", "filters=%3B%3B"] {
            let q = scope_query("a.com", seg, &uri(blank)).unwrap();
            assert!(q.contains("filters=countries%3D%3DDE"), "{blank}: {q}");
        }
    }

    #[test]
    fn test_viewer_text_cannot_alter_how_the_segment_parses() {
        let seg = "countries==DE";
        // Whatever the viewer sends, parsing the combined string yields the
        // segment's filter first and unchanged.
        for evil in [
            "countries!=DE",
            ";;;",
            "x",
            "countries==DE;countries!=DE",
            "pages==/a\n;browsers==x",
            "\u{0}",
        ] {
            let q = scope_query(
                "a.com",
                seg,
                &uri(&format!("filters={}", {
                    let mut s = String::new();
                    encode_component(evil, &mut s);
                    s
                })),
            )
            .unwrap();
            let combined = q
                .split('&')
                .find_map(|p| p.strip_prefix("filters="))
                .map(|v| {
                    axum::extract::Query::<Vec<(String, String)>>::try_from_uri(
                        &format!("/?filters={v}").parse().unwrap(),
                    )
                    .unwrap()
                    .0[0]
                        .1
                        .clone()
                })
                .unwrap();
            assert!(
                combined.starts_with("countries==DE"),
                "{evil:?} -> {combined:?}"
            );
            if let Ok(parsed) = stats::parse_filters(&combined) {
                assert_eq!(parsed[0].value, "DE");
                assert!(!parsed[0].negated);
            }
        }
    }

    #[test]
    fn test_other_parameters_pass_through_encoded() {
        let q = scope_query(
            "a.com",
            "",
            &uri("steps=page%3A%2F%2Cevent%3Asignup&window=1+day"),
        )
        .unwrap();
        assert_eq!(
            q,
            "site_id=a.com&steps=page%3A%2F%2Cevent%3Asignup&window=1%20day"
        );
    }

    #[test]
    fn test_rewritten_query_reads_back_identically() {
        // What the gate verified is what the handler's parser receives.
        let q =
            scope_query("a.com", "pages==/a b;x", &uri("foo=%C3%A9%26&bar=1")).unwrap_or_default();
        let rebuilt: Uri = format!("/p?{q}").parse().unwrap();
        let pairs = axum::extract::Query::<Vec<(String, String)>>::try_from_uri(&rebuilt)
            .unwrap()
            .0;
        assert!(pairs.contains(&("site_id".into(), "a.com".into())));
        assert!(pairs.contains(&("foo".into(), "é&".into())));
        assert!(pairs.contains(&("filters".into(), "pages==/a b;x".into())));
    }

    // ── log redaction ──

    #[test]
    fn test_redaction() {
        let token = format!("ms_{}", "ab12".repeat(16));
        for text in [
            format!("/s/{token}"),
            format!("/api/shared/stats/main?site_id=a.com&t={token}&x=1"),
            format!("{token}{token}"),
            format!("ms_{}", "a".repeat(8)),
        ] {
            let out = redact_tokens(&text);
            assert!(!out.contains("ab12ab12"), "{out}");
            assert!(out.contains("ms_[redacted]"), "{out}");
        }
        assert_eq!(
            redact_tokens("/api/stats/main?site_id=ms_site"),
            "/api/stats/main?site_id=ms_site"
        );
        assert_eq!(redact_tokens("café/ms_"), "café/ms_");
        assert_eq!(redact_tokens("plain"), "plain");
    }

    #[test]
    fn test_redaction_catches_case_and_percent_encoding() {
        // Found by an adversarial review: only the exact lowercase `ms_` was
        // redacted, so a token pasted with another case or escaped slipped
        // through into the debug log.
        let hex = "ab12".repeat(16);
        for text in [
            format!("/api/shared/stats/main?x=MS_{hex}"),
            format!("/api/shared/stats/main?x=Ms_{hex}"),
            format!("/p/%6d%73_{hex}"),
            format!("/p/%6D%73%5F{hex}"),
            format!("/p/%4d%53_{hex}"),
        ] {
            let out = redact_tokens(&text);
            assert!(!out.contains("ab12ab12"), "{text} -> {out}");
            assert!(out.contains("ms_[redacted]"), "{text} -> {out}");
        }
        // Ordinary escapes and a stray percent sign are left alone.
        assert_eq!(redact_tokens("/a%20b?q=100%"), "/a%20b?q=100%");
        assert_eq!(redact_tokens("/%zz/%4"), "/%zz/%4");
    }
}
