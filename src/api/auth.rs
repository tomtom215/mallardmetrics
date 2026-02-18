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

/// Validate that the request origin is allowed for event ingestion.
pub fn validate_origin(origin: Option<&str>, allowed_sites: &[String]) -> bool {
    if allowed_sites.is_empty() {
        return true; // No restrictions configured
    }

    origin.is_none_or(|origin| {
        let host = origin
            .strip_prefix("https://")
            .or_else(|| origin.strip_prefix("http://"))
            .unwrap_or(origin);
        allowed_sites.iter().any(|s| host.starts_with(s.as_str()))
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
    pub fn validate_key(&self, plaintext_key: &str) -> Option<ApiKeyScope> {
        let key_hash = hash_api_key(plaintext_key);
        let keys = self.keys.lock();
        keys.iter()
            .find(|k| k.key_hash == key_hash && !k.revoked)
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
/// Returns a session cookie on success.
pub async fn auth_login(
    State(state): State<Arc<AppState>>,
    Json(body): Json<PasswordRequest>,
) -> impl IntoResponse {
    let hash_guard = state.admin_password_hash.lock();
    let Some(ref stored_hash) = *hash_guard else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "No admin password configured. Use /api/auth/setup first."})),
        )
            .into_response();
    };

    if !verify_password(&body.password, stored_hash) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Invalid password"})),
        )
            .into_response();
    }
    drop(hash_guard);

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

/// POST /api/auth/logout — Invalidate the current session.
pub async fn auth_logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Some(token) = extract_session_token(&headers) {
        state.sessions.remove_session(&token);
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
) -> impl IntoResponse {
    if state.api_keys.revoke_key(&key_hash) {
        (
            StatusCode::OK,
            Json(serde_json::json!({"status": "revoked"})),
        )
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "Key not found"})),
        )
    }
}

// --- Auth Middleware ---

/// Middleware that requires authentication for protected routes.
///
/// Authentication is bypassed when no admin password is configured (open access mode).
/// Accepts either a session cookie (`mm_session`) or an API key (`Authorization: Bearer mm_...`).
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

    if is_authenticated(&state, &headers) {
        return Ok(next.run(request).await);
    }

    Err(StatusCode::UNAUTHORIZED)
}

// --- Helper Functions ---

/// Check if a request is authenticated via session cookie or API key.
fn is_authenticated(state: &AppState, headers: &HeaderMap) -> bool {
    // Check session cookie
    if let Some(token) = extract_session_token(headers) {
        if state.sessions.validate_session(&token).is_some() {
            return true;
        }
    }

    // Check API key in Authorization header
    if let Some(auth) = headers.get("authorization") {
        if let Ok(auth_str) = auth.to_str() {
            if let Some(key) = auth_str.strip_prefix("Bearer ") {
                if state.api_keys.validate_key(key).is_some() {
                    return true;
                }
            }
        }
    }

    false
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
}
