use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-IP login attempt tracker for brute-force protection.
///
/// Tracks failed login attempts and locks out IPs that exceed the configured
/// maximum. A capacity of 0 disables tracking (all requests are allowed).
#[derive(Clone)]
pub struct LoginAttemptTracker {
    attempts: Arc<Mutex<HashMap<String, LoginAttemptEntry>>>,
    max_attempts: u32,
    lockout_secs: u64,
}

struct LoginAttemptEntry {
    fail_count: u32,
    lockout_until: Option<Instant>,
}

impl LoginAttemptTracker {
    /// Create a new tracker. `max_attempts = 0` disables brute-force protection.
    pub fn new(max_attempts: u32, lockout_secs: u64) -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
            max_attempts,
            lockout_secs,
        }
    }

    /// Check whether the IP is currently locked out.
    /// Returns `true` if the request should be allowed, `false` if locked out.
    pub fn check(&self, ip: &str) -> bool {
        if self.max_attempts == 0 {
            return true;
        }
        // Inner block ensures the mutex guard is dropped before we return.
        let is_locked_out = {
            let mut map = self.attempts.lock();
            if let Some(entry) = map.get_mut(ip) {
                if let Some(until) = entry.lockout_until {
                    if Instant::now() < until {
                        true
                    } else {
                        // Lockout expired — reset
                        entry.fail_count = 0;
                        entry.lockout_until = None;
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        };
        !is_locked_out
    }

    /// Record a failed login attempt for the IP.
    /// Returns the current failure count after recording.
    pub fn record_failure(&self, ip: &str) -> u32 {
        if self.max_attempts == 0 {
            return 0;
        }
        // Inner block ensures the mutex guard is dropped before the tracing call.
        let fail_count = {
            let mut map = self.attempts.lock();
            let entry = map.entry(ip.to_string()).or_insert(LoginAttemptEntry {
                fail_count: 0,
                lockout_until: None,
            });
            entry.fail_count += 1;
            if entry.fail_count >= self.max_attempts {
                entry.lockout_until = Some(Instant::now() + Duration::from_secs(self.lockout_secs));
            }
            let fc = entry.fail_count;
            // NLL: entry borrow ends here; explicitly drop the guard before tracing.
            drop(map);
            fc
        };
        if fail_count >= self.max_attempts {
            tracing::warn!(
                ip_prefix = %anonymize_ip(ip),
                fail_count,
                lockout_secs = self.lockout_secs,
                "Login brute-force lockout applied"
            );
        }
        fail_count
    }

    /// Clear failure history for an IP (called on successful login).
    pub fn record_success(&self, ip: &str) {
        if self.max_attempts == 0 {
            return;
        }
        self.attempts.lock().remove(ip);
    }

    /// Returns the remaining lockout duration in seconds for the IP, or `None` if not locked out.
    pub fn remaining_lockout_secs(&self, ip: &str) -> Option<u64> {
        if self.max_attempts == 0 {
            return None;
        }
        let map = self.attempts.lock();
        map.get(ip).and_then(|entry| {
            entry.lockout_until.and_then(|until| {
                let now = Instant::now();
                if until > now {
                    Some(until.saturating_duration_since(now).as_secs().max(1))
                } else {
                    None
                }
            })
        })
    }

    /// Remove stale entries to prevent memory growth.
    pub fn cleanup(&self) {
        let now = Instant::now();
        self.attempts.lock().retain(|_, entry| {
            entry.lockout_until.is_some_and(|until| until > now) || entry.fail_count > 0
        });
    }
}

/// Anonymize an IP address for logging (replaces the last octet/segment).
fn anonymize_ip(ip: &str) -> String {
    if ip.contains(':') {
        // IPv6 — keep only first 4 groups
        let groups: Vec<&str> = ip.split(':').collect();
        format!("{}:...", groups.first().copied().unwrap_or("?"))
    } else {
        // IPv4 — replace last octet with 'x'
        let octets: Vec<&str> = ip.split('.').collect();
        match octets.as_slice() {
            [a, b, c, _] => format!("{a}.{b}.{c}.x"),
            _ => "?.?.?.x".to_string(),
        }
    }
}

/// Validate that the request origin is allowed for event ingestion.
///
/// Extracts the host (authority) from the Origin header and compares it exactly
/// against each allowed site. A port suffix is permitted (e.g. `example.com:8080`
/// matches the allowed entry `"example.com"`), but a leading prefix match is
/// explicitly rejected to prevent bypass via domains such as `example.com.evil.com`.
pub fn validate_origin(origin: Option<&str>, allowed_sites: &[String]) -> bool {
    if allowed_sites.is_empty() {
        return true; // No restrictions configured
    }

    origin.is_none_or(|origin| {
        // Strip scheme to obtain the authority (host[:port]) portion only.
        // HTTP Origins never contain a path component, so splitting on '/' is
        // not strictly required, but we do it defensively.
        let authority = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .unwrap_or(origin)
            .split('/')
            .next()
            .unwrap_or(origin);

        // Exact match or match with an explicit port suffix.
        // "example.com.evil.com" does NOT match "example.com".
        allowed_sites
            .iter()
            .any(|s| authority == s.as_str() || authority.starts_with(&format!("{s}:")))
    })
}

/// Hash a password using Argon2id (OWASP recommended).
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    use rand::Rng;
    let salt_bytes: [u8; 16] = rand::rng().random();
    let salt = SaltString::encode_b64(&salt_bytes)?;
    let argon2 = Argon2::default();
    let hash = argon2.hash_password(password.as_bytes(), &salt)?;
    Ok(hash.to_string())
}

/// Verify a password against an Argon2id hash.
/// Uses timing-safe comparison (provided by argon2 crate).
pub fn verify_password(password: &str, hash: &str) -> bool {
    let Ok(parsed_hash) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed_hash)
        .is_ok()
}

/// Generate a cryptographically random session token (256 bits).
pub fn generate_session_token() -> String {
    use rand::Rng;
    let token_bytes: [u8; 32] = rand::rng().random();
    hex::encode(token_bytes)
}

/// Generate a cryptographically random API key (256 bits).
pub fn generate_api_key() -> String {
    use rand::Rng;
    let key_bytes: [u8; 32] = rand::rng().random();
    format!("mm_{}", hex::encode(key_bytes))
}

/// Hash an API key for storage at rest using SHA-256.
pub fn hash_api_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
}

/// Constant-time byte slice comparison to prevent timing attacks.
///
/// Always compares all bytes regardless of where the first mismatch occurs,
/// preventing attackers from inferring hash prefixes via response timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// API key scope defining access level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ApiKeyScope {
    /// Read-only access to stats queries.
    ReadOnly,
    /// Full admin access (user management, config).
    Admin,
}

/// Stored API key metadata.
#[derive(Debug, Clone)]
pub struct StoredApiKey {
    pub key_hash: String,
    pub name: String,
    pub scope: ApiKeyScope,
    pub created_at: chrono::NaiveDateTime,
    pub revoked: bool,
}

/// Thread-safe session store for dashboard authentication.
#[derive(Clone)]
pub struct SessionStore {
    /// Maps session token → (username, expiry).
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
    ttl: Duration,
}

struct SessionEntry {
    username: String,
    expires_at: Instant,
}

impl SessionStore {
    pub fn new(ttl_secs: u64) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_secs),
        }
    }

    /// Returns the TTL in seconds.
    pub const fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    /// Create a new session for a user. Returns the session token.
    pub fn create_session(&self, username: &str) -> String {
        let token = generate_session_token();
        let entry = SessionEntry {
            username: username.to_string(),
            expires_at: Instant::now() + self.ttl,
        };
        self.sessions.lock().insert(token.clone(), entry);
        token
    }

    /// Validate a session token. Returns the username if valid and not expired.
    pub fn validate_session(&self, token: &str) -> Option<String> {
        let mut sessions = self.sessions.lock();
        if let Some(entry) = sessions.get(token) {
            if entry.expires_at > Instant::now() {
                return Some(entry.username.clone());
            }
            // Expired — remove it
            sessions.remove(token);
        }
        None
    }

    /// Remove a session (logout).
    pub fn remove_session(&self, token: &str) {
        self.sessions.lock().remove(token);
    }

    /// Remove all expired sessions (housekeeping).
    pub fn cleanup_expired(&self) {
        let now = Instant::now();
        self.sessions
            .lock()
            .retain(|_, entry| entry.expires_at > now);
    }
}

/// Thread-safe API key store.
#[derive(Clone)]
pub struct ApiKeyStore {
    keys: Arc<Mutex<Vec<StoredApiKey>>>,
}

impl Default for ApiKeyStore {
    fn default() -> Self {
        Self {
            keys: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl ApiKeyStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store a new API key (hashed). Returns the key hash for identification.
    pub fn add_key(&self, name: &str, plaintext_key: &str, scope: ApiKeyScope) -> String {
        let key_hash = hash_api_key(plaintext_key);
        let stored = StoredApiKey {
            key_hash: key_hash.clone(),
            name: name.to_string(),
            scope,
            created_at: chrono::Utc::now().naive_utc(),
            revoked: false,
        };
        self.keys.lock().push(stored);
        key_hash
    }

    /// Validate an API key. Returns the scope if valid and not revoked.
    ///
    /// Uses constant-time comparison of hash digests to prevent timing attacks.
    pub fn validate_key(&self, plaintext_key: &str) -> Option<ApiKeyScope> {
        let key_hash = hash_api_key(plaintext_key);
        let keys = self.keys.lock();
        keys.iter()
            .find(|k| constant_time_eq(k.key_hash.as_bytes(), key_hash.as_bytes()) && !k.revoked)
            .map(|k| k.scope)
    }

    /// Revoke an API key by hash.
    pub fn revoke_key(&self, key_hash: &str) -> bool {
        let found = self
            .keys
            .lock()
            .iter_mut()
            .find(|k| k.key_hash == key_hash)
            .is_some_and(|key| {
                key.revoked = true;
                true
            });
        found
    }

    /// List all keys (without plaintext).
    pub fn list_keys(&self) -> Vec<StoredApiKey> {
        self.keys.lock().clone()
    }

    /// Remove all revoked keys from memory.
    ///
    /// Safe to call periodically to prevent unbounded growth in long-running
    /// deployments that rotate keys frequently.
    pub fn cleanup_revoked(&self) {
        self.keys.lock().retain(|k| !k.revoked);
    }
}

// --- HTTP Handler Types ---

/// Request body for login and setup endpoints.
#[derive(Debug, Deserialize)]
pub struct PasswordRequest {
    pub password: String,
}

/// Response from the auth status endpoint.
#[derive(Debug, Serialize)]
pub struct AuthStatusResponse {
    pub setup_required: bool,
    pub authenticated: bool,
}

/// Response from login/setup containing session info.
#[derive(Debug, Serialize)]
struct LoginResponse {
    token: String,
}

// --- HTTP Handlers ---

use crate::ingest::handler::AppState;

/// POST /api/auth/setup — Set the initial admin password.
///
/// Only works when no admin password has been configured yet.
/// After setup, all stats/dashboard routes require authentication.
pub async fn auth_setup(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PasswordRequest>,
) -> impl IntoResponse {
    if body.password.len() < 8 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Password must be at least 8 characters"})),
        )
            .into_response();
    }

    let mut hash_guard = state.admin_password_hash.lock();
    if hash_guard.is_some() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error": "Admin password already configured"})),
        )
            .into_response();
    }

    let hash = match hash_password(&body.password) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "Failed to hash password during setup");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Internal error"})),
            )
                .into_response();
        }
    };

    *hash_guard = Some(hash);
    drop(hash_guard);

    tracing::info!("Admin password configured via setup endpoint");

    // Create a session for the newly set-up admin
    let token = state.sessions.create_session("admin");
    let cookie = build_session_cookie(
        &token,
        state.sessions.ttl_secs(),
        state.dashboard_origin.as_ref(),
    );

    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!(LoginResponse { token })),
    )
        .into_response()
}

/// POST /api/auth/login — Authenticate with the admin password.
///
/// Returns a session cookie on success. Applies per-IP brute-force protection.
pub async fn auth_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PasswordRequest>,
) -> impl IntoResponse {
    let ip = extract_client_ip(&headers);

    // Brute-force check
    if !state.login_attempt_tracker.check(&ip) {
        let remaining = state
            .login_attempt_tracker
            .remaining_lockout_secs(&ip)
            .unwrap_or(1);
        tracing::warn!(
            ip_prefix = %anonymize_ip(&ip),
            remaining_secs = remaining,
            "Login attempt from locked-out IP"
        );
        let mut response = (
            StatusCode::TOO_MANY_REQUESTS,
            Json(serde_json::json!({"error": "Too many failed login attempts. Try again later."})),
        )
            .into_response();
        if let Ok(retry_val) = axum::http::HeaderValue::from_str(&remaining.to_string()) {
            response.headers_mut().insert("retry-after", retry_val);
        }
        return response;
    }

    let hash_guard = state.admin_password_hash.lock();
    let Some(ref stored_hash) = *hash_guard else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "No admin password configured. Use /api/auth/setup first."})),
        )
            .into_response();
    };

    if !verify_password(&body.password, stored_hash) {
        drop(hash_guard);
        let fail_count = state.login_attempt_tracker.record_failure(&ip);
        tracing::warn!(
            ip_prefix = %anonymize_ip(&ip),
            fail_count,
            "Admin login failed: invalid password"
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid password"})),
        )
            .into_response();
    }
    drop(hash_guard);

    state.login_attempt_tracker.record_success(&ip);

    let token = state.sessions.create_session("admin");
    tracing::info!(ip_prefix = %anonymize_ip(&ip), "Admin login successful");

    let cookie = build_session_cookie(
        &token,
        state.sessions.ttl_secs(),
        state.dashboard_origin.as_ref(),
    );

    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!(LoginResponse { token })),
    )
        .into_response()
}

/// POST /api/auth/logout — Invalidate the current session.
pub async fn auth_logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = extract_session_token(&headers) {
        state.sessions.remove_session(&token);
        tracing::info!("Admin session logged out");
    }

    // Clear the cookie
    let cookie = "mm_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0".to_string();
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"status": "logged_out"})),
    )
}

/// GET /api/auth/status — Check authentication state.
///
/// Returns whether setup is needed and whether the current request is authenticated.
pub async fn auth_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let setup_required = state.admin_password_hash.lock().is_none();
    let authenticated = if setup_required {
        true // No password = open access
    } else {
        is_authenticated(&state, &headers)
    };

    Json(AuthStatusResponse {
        setup_required,
        authenticated,
    })
}

// --- API Key Management Handlers ---

/// Request body for creating an API key.
#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub scope: ApiKeyScope,
}

/// Response from API key creation (includes plaintext key, shown only once).
#[derive(Debug, Serialize)]
struct CreateApiKeyResponse {
    key: String,
    key_hash: String,
    name: String,
    scope: ApiKeyScope,
}

/// Response item for listing API keys (no plaintext).
#[derive(Debug, Serialize)]
struct ApiKeyListItem {
    key_hash: String,
    name: String,
    scope: ApiKeyScope,
    created_at: String,
    revoked: bool,
}

/// POST /api/keys — Create a new API key (requires admin session).
pub async fn create_api_key(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateApiKeyRequest>,
) -> impl IntoResponse {
    if body.name.is_empty() || body.name.len() > 128 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Key name must be 1-128 characters"})),
        )
            .into_response();
    }

    let plaintext_key = generate_api_key();
    let key_hash = state
        .api_keys
        .add_key(&body.name, &plaintext_key, body.scope);

    tracing::info!(
        name = %body.name,
        scope = ?body.scope,
        key_hash_prefix = %&key_hash[..8],
        "API key created"
    );

    (
        StatusCode::CREATED,
        Json(serde_json::json!(CreateApiKeyResponse {
            key: plaintext_key,
            key_hash,
            name: body.name,
            scope: body.scope,
        })),
    )
        .into_response()
}

/// GET /api/keys — List all API keys (requires admin session).
pub async fn list_api_keys(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let keys: Vec<ApiKeyListItem> = state
        .api_keys
        .list_keys()
        .into_iter()
        .map(|k| ApiKeyListItem {
            key_hash: k.key_hash,
            name: k.name,
            scope: k.scope,
            created_at: k.created_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            revoked: k.revoked,
        })
        .collect();
    Json(keys)
}

/// DELETE /api/keys/:key_hash — Revoke an API key (requires admin session).
pub async fn revoke_api_key_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(key_hash): axum::extract::Path<String>,
) -> Result<impl IntoResponse, crate::api::errors::ApiError> {
    if state.api_keys.revoke_key(&key_hash) {
        tracing::info!(key_hash_prefix = %key_hash.get(..8).unwrap_or(&key_hash), "API key revoked");
        Ok((
            StatusCode::OK,
            Json(serde_json::json!({"status": "revoked"})),
        ))
    } else {
        Err(crate::api::errors::ApiError::NotFound(
            "Key not found".to_string(),
        ))
    }
}

// --- Auth Middleware ---

/// Authentication result with scope information.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthInfo {
    /// Not authenticated.
    None,
    /// Authenticated via session cookie.
    Session,
    /// Authenticated via API key with a specific scope.
    ApiKey(ApiKeyScope),
}

/// Determine authentication status and scope from a request.
fn get_auth_info(state: &AppState, headers: &HeaderMap) -> AuthInfo {
    // Check session cookie first
    if let Some(token) = extract_session_token(headers) {
        if state.sessions.validate_session(&token).is_some() {
            return AuthInfo::Session;
        }
    }

    // Check Authorization: Bearer <key>
    if let Some(auth) = headers.get("authorization") {
        if let Ok(auth_str) = auth.to_str() {
            if let Some(key) = auth_str.strip_prefix("Bearer ") {
                if let Some(scope) = state.api_keys.validate_key(key) {
                    return AuthInfo::ApiKey(scope);
                }
            }
        }
    }

    // Check X-API-Key: <key> (conventional enterprise header)
    if let Some(api_key_header) = headers.get("x-api-key") {
        if let Ok(key) = api_key_header.to_str() {
            if let Some(scope) = state.api_keys.validate_key(key) {
                return AuthInfo::ApiKey(scope);
            }
        }
    }

    AuthInfo::None
}

/// Validate that the request Origin or Referer matches the configured dashboard origin.
///
/// This prevents CSRF attacks on session-authenticated state-changing endpoints.
/// Only enforced when `dashboard_origin` is configured.
fn validate_csrf_origin(headers: &HeaderMap, dashboard_origin: Option<&String>) -> bool {
    let Some(expected) = dashboard_origin else {
        return true; // No restriction configured
    };

    if let Some(origin) = headers.get("origin") {
        if let Ok(origin_str) = origin.to_str() {
            return origin_str == expected.as_str();
        }
        return false;
    }

    if let Some(referer) = headers.get("referer") {
        if let Ok(referer_str) = referer.to_str() {
            return referer_str.starts_with(expected.as_str());
        }
        return false;
    }

    // No Origin/Referer header — allow (server-side / non-browser requests)
    true
}

/// Middleware that requires authentication for protected routes.
///
/// Authentication is bypassed when no admin password is configured (open access mode).
/// Accepts a session cookie (`mm_session`), `Authorization: Bearer mm_...`, or `X-API-Key: mm_...`.
pub async fn require_auth(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // No password configured = open access
    if state.admin_password_hash.lock().is_none() {
        return Ok(next.run(request).await);
    }

    if get_auth_info(&state, &headers) != AuthInfo::None {
        return Ok(next.run(request).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

/// Middleware that requires **admin-level** authentication for key management routes.
///
/// - Read-only API keys are rejected with 403 Forbidden.
/// - Session-authenticated requests are CSRF-checked against `dashboard_origin`.
/// - Open-access mode (no password configured) bypasses all checks.
pub async fn require_admin_auth(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if state.admin_password_hash.lock().is_none() {
        return Ok(next.run(request).await);
    }

    match get_auth_info(&state, &headers) {
        AuthInfo::None => Err(StatusCode::UNAUTHORIZED),
        AuthInfo::Session => {
            // CSRF check: Origin must match dashboard_origin when configured
            if !validate_csrf_origin(&headers, state.dashboard_origin.as_ref()) {
                tracing::warn!("CSRF check failed on admin endpoint");
                return Err(StatusCode::FORBIDDEN);
            }
            Ok(next.run(request).await)
        }
        AuthInfo::ApiKey(ApiKeyScope::Admin) => Ok(next.run(request).await),
        AuthInfo::ApiKey(ApiKeyScope::ReadOnly) => {
            tracing::warn!("ReadOnly API key attempted to access admin-only endpoint");
            Err(StatusCode::FORBIDDEN)
        }
    }
}

// --- Helper Functions ---

/// Check if a request is authenticated (any valid credential).
///
/// Returns true for sessions and any valid API key (read-only or admin).
/// Use `get_auth_info` when scope information is needed.
fn is_authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    get_auth_info(state, headers) != AuthInfo::None
}

/// Extract the client IP address from request headers.
///
/// Checks `X-Forwarded-For` first (proxy/load-balancer), then `X-Real-IP`,
/// falling back to "unknown" when no IP header is present.
fn extract_client_ip(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(str::trim)
        .or_else(|| headers.get("x-real-ip").and_then(|v| v.to_str().ok()))
        .unwrap_or("unknown")
        .to_string()
}

/// Extract session token from cookie header.
fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(token) = part.strip_prefix("mm_session=") {
            if !token.is_empty() {
                return Some(token.to_string());
            }
        }
    }
    None
}

/// Build a Set-Cookie header value for a session token.
fn build_session_cookie(token: &str, ttl_secs: u64, dashboard_origin: Option<&String>) -> String {
    let secure = dashboard_origin.is_some_and(|o| o.starts_with("https://"));
    let mut cookie =
        format!("mm_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={ttl_secs}");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_origin_no_restrictions() {
        assert!(validate_origin(Some("https://anything.com"), &[]));
    }

    #[test]
    fn test_validate_origin_allowed() {
        let sites = vec!["example.com".to_string()];
        assert!(validate_origin(Some("https://example.com"), &sites));
    }

    #[test]
    fn test_validate_origin_not_allowed() {
        let sites = vec!["example.com".to_string()];
        assert!(!validate_origin(Some("https://evil.com"), &sites));
    }

    #[test]
    fn test_validate_origin_no_header() {
        let sites = vec!["example.com".to_string()];
        assert!(validate_origin(None, &sites));
    }

    #[test]
    fn test_validate_origin_http() {
        let sites = vec!["example.com".to_string()];
        assert!(validate_origin(Some("http://example.com"), &sites));
    }

    #[test]
    fn test_validate_origin_with_port() {
        let sites = vec!["example.com".to_string()];
        assert!(validate_origin(Some("http://example.com:3000"), &sites));
    }

    #[test]
    fn test_validate_origin_prefix_bypass_rejected() {
        // "example.com.evil.com" must NOT match the allowed site "example.com".
        let sites = vec!["example.com".to_string()];
        assert!(!validate_origin(
            Some("https://example.com.evil.com"),
            &sites
        ));
    }

    #[test]
    fn test_validate_origin_prefix_subdomain_bypass_rejected() {
        // "example.com-other.io" must NOT match "example.com".
        let sites = vec!["example.com".to_string()];
        assert!(!validate_origin(
            Some("https://example.com-other.io"),
            &sites
        ));
    }

    // Password hashing tests
    #[test]
    fn test_hash_password_and_verify() {
        let password = "secure-password-123";
        let hash = hash_password(password).unwrap();
        assert!(verify_password(password, &hash));
    }

    #[test]
    fn test_verify_password_wrong() {
        let hash = hash_password("correct-password").unwrap();
        assert!(!verify_password("wrong-password", &hash));
    }

    #[test]
    fn test_hash_password_unique_salts() {
        let h1 = hash_password("same-password").unwrap();
        let h2 = hash_password("same-password").unwrap();
        assert_ne!(h1, h2, "Different salts should produce different hashes");
        assert!(verify_password("same-password", &h1));
        assert!(verify_password("same-password", &h2));
    }

    #[test]
    fn test_verify_password_invalid_hash() {
        assert!(!verify_password("any", "not-a-valid-hash"));
    }

    // Session management tests
    #[test]
    fn test_session_create_and_validate() {
        let store = SessionStore::new(3600);
        let token = store.create_session("admin");
        assert!(store.validate_session(&token).is_some());
        assert_eq!(store.validate_session(&token).unwrap(), "admin");
    }

    #[test]
    fn test_session_invalid_token() {
        let store = SessionStore::new(3600);
        assert!(store.validate_session("nonexistent-token").is_none());
    }

    #[test]
    fn test_session_remove() {
        let store = SessionStore::new(3600);
        let token = store.create_session("admin");
        store.remove_session(&token);
        assert!(store.validate_session(&token).is_none());
    }

    #[test]
    fn test_session_expiry() {
        let store = SessionStore::new(0); // 0 second TTL
        let token = store.create_session("admin");
        // Session should be expired immediately
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(store.validate_session(&token).is_none());
    }

    #[test]
    fn test_generate_session_token_length() {
        let token = generate_session_token();
        assert_eq!(token.len(), 64); // 32 bytes = 64 hex chars
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // API key tests
    #[test]
    fn test_generate_api_key_format() {
        let key = generate_api_key();
        assert!(key.starts_with("mm_"));
        assert_eq!(key.len(), 67); // "mm_" + 64 hex chars
    }

    #[test]
    fn test_hash_api_key_deterministic() {
        let h1 = hash_api_key("mm_abc123");
        let h2 = hash_api_key("mm_abc123");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_api_key_store_add_and_validate() {
        let store = ApiKeyStore::new();
        let key = generate_api_key();
        store.add_key("test-key", &key, ApiKeyScope::ReadOnly);
        assert_eq!(store.validate_key(&key), Some(ApiKeyScope::ReadOnly));
    }

    #[test]
    fn test_api_key_store_invalid_key() {
        let store = ApiKeyStore::new();
        assert!(store.validate_key("invalid-key").is_none());
    }

    #[test]
    fn test_api_key_store_revoke() {
        let store = ApiKeyStore::new();
        let key = generate_api_key();
        let key_hash = store.add_key("test-key", &key, ApiKeyScope::Admin);
        assert!(store.validate_key(&key).is_some());
        store.revoke_key(&key_hash);
        assert!(store.validate_key(&key).is_none());
    }

    #[test]
    fn test_api_key_store_scope_distinction() {
        let store = ApiKeyStore::new();
        let readonly_key = generate_api_key();
        let admin_key = generate_api_key();
        store.add_key("read", &readonly_key, ApiKeyScope::ReadOnly);
        store.add_key("admin", &admin_key, ApiKeyScope::Admin);
        assert_eq!(
            store.validate_key(&readonly_key),
            Some(ApiKeyScope::ReadOnly)
        );
        assert_eq!(store.validate_key(&admin_key), Some(ApiKeyScope::Admin));
    }

    #[test]
    fn test_api_key_store_list() {
        let store = ApiKeyStore::new();
        let key = generate_api_key();
        store.add_key("my-key", &key, ApiKeyScope::ReadOnly);
        let keys = store.list_keys();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "my-key");
        assert!(!keys[0].revoked);
    }

    #[test]
    fn test_session_cleanup_expired() {
        let store = SessionStore::new(0); // 0 second TTL
        store.create_session("user1");
        store.create_session("user2");
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.cleanup_expired();
        // All sessions should be cleaned up
        assert_eq!(store.sessions.lock().len(), 0);
    }

    // Session cookie Secure flag tests
    #[test]
    fn test_session_cookie_includes_secure_for_https_origin() {
        let cookie = build_session_cookie(
            "token123",
            3600,
            Some(&"https://analytics.example.com".to_string()),
        );
        assert!(
            cookie.contains("; Secure"),
            "Cookie should include Secure flag for HTTPS origin"
        );
    }

    #[test]
    fn test_session_cookie_omits_secure_for_http_origin() {
        let cookie =
            build_session_cookie("token123", 3600, Some(&"http://localhost:8000".to_string()));
        assert!(
            !cookie.contains("; Secure"),
            "Cookie must NOT include Secure flag for HTTP origin"
        );
    }

    #[test]
    fn test_session_cookie_omits_secure_with_no_origin() {
        let cookie = build_session_cookie("token123", 3600, None);
        assert!(
            !cookie.contains("; Secure"),
            "Cookie must NOT include Secure flag when no origin is configured"
        );
    }

    // ApiKeyStore cleanup_revoked tests
    #[test]
    fn test_api_key_store_cleanup_revoked() {
        let store = ApiKeyStore::new();
        let key1 = generate_api_key();
        let key2 = generate_api_key();
        let hash1 = store.add_key("key1", &key1, ApiKeyScope::ReadOnly);
        store.add_key("key2", &key2, ApiKeyScope::Admin);
        // Revoke key1
        store.revoke_key(&hash1);
        assert_eq!(store.list_keys().len(), 2);
        // Cleanup removes revoked key
        store.cleanup_revoked();
        let remaining = store.list_keys();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "key2");
    }

    // LoginAttemptTracker tests
    #[test]
    fn test_login_tracker_disabled_when_max_zero() {
        let tracker = LoginAttemptTracker::new(0, 300);
        // Always allowed when disabled
        for _ in 0..100 {
            assert!(tracker.check("1.2.3.4"));
            tracker.record_failure("1.2.3.4");
        }
    }

    #[test]
    fn test_login_tracker_allows_below_limit() {
        let tracker = LoginAttemptTracker::new(5, 300);
        // 4 failures should still be allowed
        for _ in 0..4 {
            assert!(tracker.check("1.2.3.4"));
            tracker.record_failure("1.2.3.4");
        }
        assert!(tracker.check("1.2.3.4"));
    }

    #[test]
    fn test_login_tracker_lockout_after_max_attempts() {
        let tracker = LoginAttemptTracker::new(3, 300);
        // Use up all 3 attempts
        tracker.record_failure("10.0.0.1");
        tracker.record_failure("10.0.0.1");
        tracker.record_failure("10.0.0.1");
        // Should now be locked out
        assert!(
            !tracker.check("10.0.0.1"),
            "IP should be locked out after 3 failures"
        );
    }

    #[test]
    fn test_login_tracker_success_clears_failures() {
        let tracker = LoginAttemptTracker::new(3, 300);
        tracker.record_failure("10.0.0.2");
        tracker.record_failure("10.0.0.2");
        tracker.record_success("10.0.0.2");
        // After success, failures are cleared
        assert!(tracker.check("10.0.0.2"));
        assert!(!tracker.attempts.lock().contains_key("10.0.0.2"));
    }

    #[test]
    fn test_login_tracker_independent_ips() {
        let tracker = LoginAttemptTracker::new(2, 300);
        // Exhaust IP-A
        tracker.record_failure("192.168.1.1");
        tracker.record_failure("192.168.1.1");
        assert!(!tracker.check("192.168.1.1"));
        // IP-B should be unaffected
        assert!(tracker.check("192.168.1.2"));
    }

    #[test]
    fn test_remaining_lockout_secs_returns_positive_when_locked() {
        let tracker = LoginAttemptTracker::new(1, 300);
        tracker.record_failure("10.0.0.7");
        // IP should be locked out; remaining should be between 1 and 300
        let remaining = tracker.remaining_lockout_secs("10.0.0.7");
        assert!(
            remaining.is_some(),
            "remaining_lockout_secs should return Some when locked out"
        );
        let secs = remaining.unwrap();
        assert!(
            (1..=300).contains(&secs),
            "remaining secs {secs} out of range"
        );
    }

    #[test]
    fn test_remaining_lockout_secs_none_when_not_locked() {
        let tracker = LoginAttemptTracker::new(3, 300);
        // No failures yet — not locked out
        assert!(tracker.remaining_lockout_secs("10.0.0.8").is_none());
    }

    #[test]
    fn test_remaining_lockout_secs_none_when_disabled() {
        let tracker = LoginAttemptTracker::new(0, 300);
        // Tracker disabled — remaining is always None
        assert!(tracker.remaining_lockout_secs("10.0.0.9").is_none());
    }

    // CSRF validation tests
    #[test]
    fn test_csrf_validate_no_dashboard_origin_allows_all() {
        let headers = HeaderMap::new();
        assert!(validate_csrf_origin(&headers, None));
    }

    #[test]
    fn test_csrf_validate_matching_origin_allowed() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", "https://analytics.example.com".parse().unwrap());
        assert!(validate_csrf_origin(
            &headers,
            Some(&"https://analytics.example.com".to_string())
        ));
    }

    #[test]
    fn test_csrf_validate_mismatching_origin_rejected() {
        let mut headers = HeaderMap::new();
        headers.insert("origin", "https://evil.com".parse().unwrap());
        assert!(!validate_csrf_origin(
            &headers,
            Some(&"https://analytics.example.com".to_string())
        ));
    }

    #[test]
    fn test_csrf_validate_no_origin_or_referer_allows() {
        // Server-side requests without Origin/Referer should be allowed
        let headers = HeaderMap::new();
        assert!(validate_csrf_origin(
            &headers,
            Some(&"https://analytics.example.com".to_string())
        ));
    }

    // X-API-Key / X-Forwarded-For helper tests
    #[test]
    fn test_extract_client_ip_x_forwarded_for() {
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.1, 10.0.0.1".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "203.0.113.1");
    }

    #[test]
    fn test_extract_client_ip_x_real_ip() {
        let mut headers = HeaderMap::new();
        headers.insert("x-real-ip", "203.0.113.2".parse().unwrap());
        assert_eq!(extract_client_ip(&headers), "203.0.113.2");
    }

    #[test]
    fn test_extract_client_ip_unknown() {
        let headers = HeaderMap::new();
        assert_eq!(extract_client_ip(&headers), "unknown");
    }

    #[test]
    fn test_anonymize_ip_v4() {
        assert_eq!(anonymize_ip("1.2.3.4"), "1.2.3.x");
        assert_eq!(anonymize_ip("192.168.1.100"), "192.168.1.x");
    }

    #[test]
    fn test_anonymize_ip_v6() {
        let result = anonymize_ip("2001:db8::1");
        assert!(result.contains("..."));
    }

    #[test]
    fn test_constant_time_eq_equal() {
        assert!(constant_time_eq(b"abcdef", b"abcdef"));
    }

    #[test]
    fn test_constant_time_eq_not_equal() {
        assert!(!constant_time_eq(b"abcdef", b"abcdeg"));
    }

    #[test]
    fn test_constant_time_eq_different_lengths() {
        assert!(!constant_time_eq(b"abc", b"abcdef"));
    }

    #[test]
    fn test_constant_time_eq_empty() {
        assert!(constant_time_eq(b"", b""));
    }
}
