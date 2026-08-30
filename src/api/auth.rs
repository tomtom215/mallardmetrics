use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Per-IP login-attempt tracker for brute-force protection.
///
/// The map is keyed by client IP, so it is bounded: without a cap an attacker
/// rotating source addresses grows it without limit. The previous `cleanup`
/// retained every entry with `fail_count > 0` — which is every entry it ever
/// created — so it freed nothing at all.
#[derive(Clone)]
pub struct LoginAttemptTracker {
    attempts: Arc<Mutex<HashMap<String, LoginAttemptEntry>>>,
    max_attempts: u32,
    lockout_secs: u64,
    max_entries: usize,
}

struct LoginAttemptEntry {
    fail_count: u32,
    lockout_until: Option<Instant>,
    last_seen: Instant,
}

/// How long a non-locked-out failure record is kept.
const ATTEMPT_RETENTION: Duration = Duration::from_secs(3600);

impl LoginAttemptTracker {
    /// Create a tracker. `max_attempts == 0` disables brute-force protection.
    pub fn new(max_attempts: u32, lockout_secs: u64) -> Self {
        Self::with_capacity(max_attempts, lockout_secs, 10_000)
    }

    pub fn with_capacity(max_attempts: u32, lockout_secs: u64, max_entries: usize) -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
            max_attempts,
            lockout_secs,
            max_entries,
        }
    }

    /// Whether the IP may attempt a login. `false` means it is locked out.
    pub fn check(&self, ip: &str) -> bool {
        if self.max_attempts == 0 {
            return true;
        }
        let mut map = self.attempts.lock();
        let Some(entry) = map.get_mut(ip) else {
            return true;
        };
        let Some(until) = entry.lockout_until else {
            return true;
        };
        if Instant::now() < until {
            return false;
        }
        // The lockout has expired — reset so the IP gets a fresh allowance.
        entry.fail_count = 0;
        entry.lockout_until = None;
        entry.last_seen = Instant::now();
        true
    }

    /// Record a failed login. Returns the failure count after recording.
    pub fn record_failure(&self, ip: &str) -> u32 {
        if self.max_attempts == 0 {
            return 0;
        }
        let now = Instant::now();
        let fail_count = {
            let mut map = self.attempts.lock();
            if !map.contains_key(ip) && map.len() >= self.max_entries {
                // Reclaim expired records before giving up on tracking this IP.
                Self::evict_stale(&mut map, now);
                if map.len() >= self.max_entries {
                    // Still full: every slot is an active lockout. Refusing to
                    // track is safe — check() already denies those addresses.
                    tracing::warn!(
                        max_entries = self.max_entries,
                        "Login attempt tracker is at capacity; not tracking a new IP"
                    );
                    return self.max_attempts;
                }
            }
            let entry = map.entry(ip.to_string()).or_insert(LoginAttemptEntry {
                fail_count: 0,
                lockout_until: None,
                last_seen: now,
            });
            entry.fail_count += 1;
            entry.last_seen = now;
            if entry.fail_count >= self.max_attempts {
                entry.lockout_until = Some(now + Duration::from_secs(self.lockout_secs));
            }
            entry.fail_count
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

    /// Clear an IP's failure history after a successful login.
    pub fn record_success(&self, ip: &str) {
        if self.max_attempts == 0 {
            return;
        }
        self.attempts.lock().remove(ip);
    }

    /// Remaining lockout in seconds, or `None` when not locked out.
    pub fn remaining_lockout_secs(&self, ip: &str) -> Option<u64> {
        if self.max_attempts == 0 {
            return None;
        }
        let map = self.attempts.lock();
        map.get(ip).and_then(|entry| {
            entry.lockout_until.and_then(|until| {
                let now = Instant::now();
                (until > now).then(|| until.saturating_duration_since(now).as_secs().max(1))
            })
        })
    }

    /// Drop entries that are neither locked out nor recently active.
    fn evict_stale(map: &mut HashMap<String, LoginAttemptEntry>, now: Instant) {
        map.retain(|_, entry| {
            entry.lockout_until.is_some_and(|until| until > now)
                || now.duration_since(entry.last_seen) < ATTEMPT_RETENTION
        });
    }

    /// Periodic housekeeping.
    pub fn cleanup(&self) {
        Self::evict_stale(&mut self.attempts.lock(), Instant::now());
    }

    /// Number of tracked IPs (for metrics and tests).
    pub fn tracked_ips(&self) -> usize {
        self.attempts.lock().len()
    }
}

/// Redact an IP address for logging.
///
/// IPv4 keeps the /24 prefix, IPv6 the /48 routing prefix — the conventional
/// truncations, and enough to tell one attacking network from another while
/// dropping the bits that identify a subscriber or host.
///
/// The address is parsed rather than split on punctuation. Textual splitting
/// cannot handle IPv6's `::` compression: `2001:db8::1` has four
/// colon-separated fields, so taking "the first four" reproduced the entire
/// address and redacted nothing. Parsing also means an unparseable value logs a
/// fixed placeholder instead of attacker-controlled text.
pub fn anonymize_ip(ip: &str) -> String {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let [a, b, c, _] = v4.octets();
            format!("{a}.{b}.{c}.x")
        }
        Ok(IpAddr::V6(v6)) => {
            let [a, b, c, ..] = v6.segments();
            format!("{a:x}:{b:x}:{c:x}::x")
        }
        Err(_) => "(unparseable)".to_string(),
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

/// Hash a password using Argon2id with the crate's OWASP-aligned defaults.
///
/// Costs roughly 50-100 ms of CPU by design, so callers on an async task must
/// run this inside `spawn_blocking` — see [`hash_password_async`].
///
/// # Errors
///
/// Returns an error if the hashing parameters are rejected or the RNG fails.
pub fn hash_password(password: &str) -> Result<String, argon2::password_hash::Error> {
    // argon2 0.6 generates the salt internally from the OS RNG; the caller no
    // longer supplies one.
    let hash: PasswordHash = Argon2::default().hash_password(password.as_bytes())?;
    Ok(hash.to_string())
}

/// Hash a password on the blocking thread pool.
///
/// Argon2 is deliberately CPU-expensive. Running it directly on an async worker
/// pins that worker for the duration, so a handful of concurrent login attempts
/// could stall every other request on the server.
///
/// # Errors
///
/// Returns an error if hashing fails or the blocking task panics.
pub async fn hash_password_async(password: String) -> Result<String, argon2::password_hash::Error> {
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .unwrap_or(Err(argon2::password_hash::Error::Crypto))
}

/// Verify a password on the blocking thread pool. See [`hash_password_async`].
pub async fn verify_password_async(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .unwrap_or(false)
}

/// Verify a password against an Argon2id PHC string.
///
/// The comparison is constant-time, courtesy of the argon2 crate.
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
    use rand::RngExt;
    let token_bytes: [u8; 32] = rand::rng().random();
    hex::encode(token_bytes)
}

/// Generate a cryptographically random API key (256 bits).
pub fn generate_api_key() -> String {
    use rand::RngExt;
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
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
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
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StoredApiKey {
    pub key_hash: String,
    pub name: String,
    pub scope: ApiKeyScope,
    pub created_at: chrono::NaiveDateTime,
    pub revoked: bool,
}

/// Thread-safe session store for dashboard authentication.
///
/// Bounded: every successful login mints a token, and nothing but the periodic
/// sweep removed them, so a long-lived instance accumulated one entry per login
/// for the whole session TTL.
#[derive(Clone)]
pub struct SessionStore {
    /// Maps session token → (username, expiry).
    sessions: Arc<Mutex<HashMap<String, SessionEntry>>>,
    ttl: Duration,
    max_sessions: usize,
}

struct SessionEntry {
    username: String,
    expires_at: Instant,
}

impl SessionStore {
    pub fn new(ttl_secs: u64) -> Self {
        Self::with_capacity(ttl_secs, 10_000)
    }

    pub fn with_capacity(ttl_secs: u64, max_sessions: usize) -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            ttl: Duration::from_secs(ttl_secs),
            max_sessions,
        }
    }

    /// Session lifetime in seconds.
    pub const fn ttl_secs(&self) -> u64 {
        self.ttl.as_secs()
    }

    /// Create a session and return its token.
    pub fn create_session(&self, username: &str) -> String {
        let token = generate_session_token();
        let now = Instant::now();
        let mut sessions = self.sessions.lock();

        if self.max_sessions > 0 && sessions.len() >= self.max_sessions {
            sessions.retain(|_, entry| entry.expires_at > now);
            while sessions.len() >= self.max_sessions {
                // Evict whichever live session expires soonest.
                let Some(victim) = sessions
                    .iter()
                    .min_by_key(|(_, e)| e.expires_at)
                    .map(|(k, _)| k.clone())
                else {
                    break;
                };
                sessions.remove(&victim);
            }
        }

        sessions.insert(
            token.clone(),
            SessionEntry {
                username: username.to_string(),
                expires_at: now + self.ttl,
            },
        );
        token
    }

    /// Validate a token, returning the username if it is live.
    pub fn validate_session(&self, token: &str) -> Option<String> {
        let mut sessions = self.sessions.lock();
        if let Some(entry) = sessions.get(token) {
            if entry.expires_at > Instant::now() {
                return Some(entry.username.clone());
            }
            sessions.remove(token);
        }
        None
    }

    /// Remove a session (logout).
    pub fn remove_session(&self, token: &str) {
        self.sessions.lock().remove(token);
    }

    /// Remove expired sessions.
    pub fn cleanup_expired(&self) {
        let now = Instant::now();
        self.sessions
            .lock()
            .retain(|_, entry| entry.expires_at > now);
    }

    /// Number of live sessions (for metrics and tests).
    pub fn len(&self) -> usize {
        self.sessions.lock().len()
    }

    /// True when no sessions are held.
    pub fn is_empty(&self) -> bool {
        self.sessions.lock().is_empty()
    }
}

/// Write `contents` to `path` atomically, readable only by the owner.
///
/// Used for both `api_keys.json` and the visitor-ID secret. Both were
/// previously written with `std::fs::write`, which truncates first (so a crash
/// mid-write loses the file) and inherits the process umask (so on a typical
/// system they landed world-readable).
///
/// # Errors
///
/// Returns an error if the file cannot be written, its permissions cannot be
/// set, or the rename into place fails.
pub fn write_private_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;

    // Same directory as the target, so the rename below stays within one
    // filesystem and is therefore atomic.
    let tmp_path = path.with_extension(format!("tmp.{}", std::process::id()));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let result = (|| -> std::io::Result<()> {
        let mut file = options.open(&tmp_path)?;
        file.write_all(contents)?;
        // Flush to the device before the rename, so a crash cannot leave a
        // correctly-named but empty file.
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    std::fs::rename(&tmp_path, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp_path);
    })
}

/// Thread-safe API key store with optional disk persistence.
///
/// When `persist_path` is set, any mutation (`add_key`, `revoke_key`) is
/// immediately written to disk as a JSON array of `StoredApiKey` records.
/// On startup the caller should use `ApiKeyStore::load_from_disk` to restore
/// keys written by previous runs.
#[derive(Clone)]
pub struct ApiKeyStore {
    keys: Arc<Mutex<Vec<StoredApiKey>>>,
    persist_path: Option<Arc<std::path::PathBuf>>,
}

impl Default for ApiKeyStore {
    fn default() -> Self {
        Self {
            keys: Arc::new(Mutex::new(Vec::new())),
            persist_path: None,
        }
    }
}

impl ApiKeyStore {
    /// Create a store that loads existing keys from `path` and persists
    /// mutations back to the same file.  Missing file is treated as empty.
    pub fn load_from_disk(path: std::path::PathBuf) -> Self {
        let keys = match std::fs::read_to_string(&path) {
            Ok(contents) => {
                serde_json::from_str::<Vec<StoredApiKey>>(&contents).unwrap_or_else(|e| {
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "Failed to parse api_keys.json; starting with empty key store"
                    );
                    Vec::new()
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "Could not read api_keys.json; starting with empty key store"
                );
                Vec::new()
            }
        };
        let count = keys.len();
        if count > 0 {
            tracing::info!(path = %path.display(), count, "Loaded API keys from disk");
        }
        Self {
            keys: Arc::new(Mutex::new(keys)),
            persist_path: Some(Arc::new(path)),
        }
    }

    /// Persist the current key set to disk.
    ///
    /// Written to a temporary file in the same directory and renamed into
    /// place, so an interrupted write cannot leave a truncated `api_keys.json`
    /// that would silently revoke every key on the next restart. The file is
    /// created with owner-only permissions: it holds the SHA-256 of each key,
    /// which is exactly what an attacker needs to mount an offline search.
    ///
    /// Never panics; failures are logged.
    fn persist(&self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        let snapshot = self.keys.lock().clone();
        let json = match serde_json::to_string_pretty(&snapshot) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to serialize API keys for persistence");
                return;
            }
        };
        if let Err(e) = write_private_file(path.as_ref(), json.as_bytes()) {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "Failed to persist API keys to disk"
            );
        }
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
        self.persist();
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
        if found {
            self.persist();
        }
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

/// Minimum admin password length.
///
/// Raised from 8. Eight characters is below every current recommendation
/// (NIST SP 800-63B asks for at least 8 for user-chosen secrets but expects
/// throttling and breach screening; OWASP suggests 12 for administrative
/// accounts), and this password guards every visitor record on the instance.
pub const MIN_PASSWORD_LEN: usize = 12;

/// Passwords automated scanners try first.
///
/// Not a substitute for breach screening, but it costs nothing and blocks the
/// handful of values that would otherwise be found within seconds.
const OBVIOUS_PASSWORDS: &[&str] = &[
    "password",
    "password123",
    "administrator",
    "changeme",
    "letmein",
    "analytics",
    "mallardmetrics",
    "123456789012",
];

/// Reject a password that is too short or obviously guessable.
fn validate_password(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(format!(
            "Password must be at least {MIN_PASSWORD_LEN} characters"
        ));
    }
    let lowered = password.to_ascii_lowercase();
    if OBVIOUS_PASSWORDS.contains(&lowered.as_str()) {
        return Err("Password is too common; choose something unpredictable".to_string());
    }
    Ok(())
}

/// Build the login/setup success response, including the session cookie.
fn session_response(state: &AppState, token: String) -> Response {
    let secure = state.secure_cookies
        || state
            .dashboard_origin
            .as_deref()
            .is_some_and(|o| o.starts_with("https://"));
    let cookie = build_session_cookie(&token, state.sessions.ttl_secs(), secure);
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!(LoginResponse { token })),
    )
        .into_response()
}

/// POST /api/auth/setup — Set the initial admin password.
///
/// Only works while no admin password is configured. Rate-limited by the same
/// per-IP tracker as login, so the window between first boot and first setup
/// cannot be brute-forced open by an automated scanner.
pub async fn auth_setup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PasswordRequest>,
) -> impl IntoResponse {
    let ip = extract_client_ip_from(&state, &headers);

    if !state.login_attempt_tracker.check(&ip) {
        return too_many_attempts(&state, &ip);
    }

    if let Err(message) = validate_password(&body.password) {
        // A rejected attempt still counts: setup is unauthenticated, so without
        // this an attacker could probe it without limit.
        state.login_attempt_tracker.record_failure(&ip);
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message })),
        )
            .into_response();
    }

    // Check-then-hash, re-checking after: hashing takes ~50-100 ms and must not
    // hold the lock, but two concurrent setup requests must not both succeed.
    if state.admin_password_hash.lock().is_some() {
        return already_configured();
    }

    let hash = match hash_password_async(body.password).await {
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

    {
        let mut guard = state.admin_password_hash.lock();
        if guard.is_some() {
            return already_configured();
        }
        *guard = Some(hash);
    }

    state.login_attempt_tracker.record_success(&ip);
    tracing::info!("Admin password configured via the setup endpoint");

    session_response(&state, state.sessions.create_session("admin"))
}

fn already_configured() -> Response {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({"error": "Admin password already configured"})),
    )
        .into_response()
}

/// 429 response carrying the remaining lockout as `Retry-After`.
fn too_many_attempts(state: &AppState, ip: &str) -> Response {
    let remaining = state
        .login_attempt_tracker
        .remaining_lockout_secs(ip)
        .unwrap_or(1);
    tracing::warn!(
        ip_prefix = %anonymize_ip(ip),
        remaining_secs = remaining,
        "Request from a locked-out IP"
    );
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({"error": "Too many failed attempts. Try again later."})),
    )
        .into_response();
    if let Ok(value) = axum::http::HeaderValue::from_str(&remaining.to_string()) {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

/// POST /api/auth/login — Authenticate with the admin password.
///
/// Returns a session cookie on success. Per-IP brute-force protection applies.
pub async fn auth_login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PasswordRequest>,
) -> impl IntoResponse {
    let ip = extract_client_ip_from(&state, &headers);

    if !state.login_attempt_tracker.check(&ip) {
        return too_many_attempts(&state, &ip);
    }

    // Clone the stored hash and release the lock immediately. Verification is
    // ~50-100 ms of Argon2 work; holding the mutex across it serialised every
    // login attempt and blocked every other reader of the hash, and running it
    // inline pinned an async worker thread for the duration.
    let stored_hash = state.admin_password_hash.lock().clone();
    let Some(stored_hash) = stored_hash else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No admin password configured. Use /api/auth/setup first."
            })),
        )
            .into_response();
    };

    if !verify_password_async(body.password, stored_hash).await {
        let fail_count = state.login_attempt_tracker.record_failure(&ip);
        state
            .login_failures_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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

    state.login_attempt_tracker.record_success(&ip);
    tracing::info!(ip_prefix = %anonymize_ip(&ip), "Admin login successful");

    session_response(&state, state.sessions.create_session("admin"))
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

    // The clearing cookie must match the attributes of the one it replaces
    // (notably Secure), or the browser treats it as a different cookie and the
    // original survives.
    let secure = state.secure_cookies
        || state
            .dashboard_origin
            .as_deref()
            .is_some_and(|o| o.starts_with("https://"));
    let mut cookie = "mm_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0".to_string();
    if secure {
        cookie.push_str("; Secure");
    }
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
        // Reads are open before setup; admin routes are not (see
        // `require_admin_auth`), so this reports read access.
        true
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
    // Session cookie first.
    if let Some(token) = extract_session_token(headers)
        && state.sessions.validate_session(&token).is_some()
    {
        return AuthInfo::Session;
    }

    // Authorization: Bearer <key>
    if let Some(auth) = headers.get("authorization")
        && let Ok(auth_str) = auth.to_str()
        && let Some(key) = auth_str.strip_prefix("Bearer ")
        && let Some(scope) = state.api_keys.validate_key(key)
    {
        return AuthInfo::ApiKey(scope);
    }

    // X-API-Key: <key> — the conventional enterprise header.
    if let Some(api_key_header) = headers.get("x-api-key")
        && let Ok(key) = api_key_header.to_str()
        && let Some(scope) = state.api_keys.validate_key(key)
    {
        return AuthInfo::ApiKey(scope);
    }

    AuthInfo::None
}

/// Extract the scheme+authority (origin) from a full Referer URL.
///
/// `"https://analytics.example.com/dashboard/page"` → `"https://analytics.example.com"`
///
/// Returns `None` if the URL does not start with `http://` or `https://`.
fn extract_origin_from_referer(referer: &str) -> Option<&str> {
    let scheme_len = if referer.starts_with("https://") {
        8 // len("https://")
    } else if referer.starts_with("http://") {
        7 // len("http://")
    } else {
        return None;
    };
    // Everything after the scheme up to the first '/' is the host[:port].
    let after_scheme = &referer[scheme_len..];
    let host_len = after_scheme.find('/').unwrap_or(after_scheme.len());
    Some(&referer[..scheme_len + host_len])
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
            // Extract only the scheme+authority from the Referer URL before comparing.
            // Using starts_with() would allow "https://example.com.evil.com/…" to bypass
            // a rule for "https://example.com".
            let referer_origin = extract_origin_from_referer(referer_str).unwrap_or("");
            return referer_origin == expected.as_str();
        }
        return false;
    }

    // No Origin/Referer header — allow (server-side / non-browser requests)
    true
}

/// Middleware that requires authentication for analytics read routes.
///
/// Accepts a session cookie (`mm_session`), `Authorization: Bearer mm_...`, or
/// `X-API-Key: mm_...`.
///
/// Before setup, reads are open — a deliberate deployment mode for a public
/// dashboard. That exemption is scoped to reads: [`require_admin_auth`] refuses
/// key management and erasure until an admin exists.
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

/// Middleware that requires **admin-level** authentication for key management
/// and data-destroying routes.
///
/// - Read-only API keys are rejected with 403 Forbidden.
/// - Session-authenticated requests are CSRF-checked against `dashboard_origin`.
/// - Before setup there is no admin, so these routes are refused outright.
///
/// Open-access mode used to bypass this check as well as [`require_auth`], which
/// left `POST /api/keys` and `DELETE /api/gdpr/erase` reachable by anyone who
/// could connect to an instance whose password had not been set yet. Minting a
/// key was the worse half: an admin key issued in that window keeps working
/// after the operator finishes setup, turning a few unconfigured minutes into
/// permanent access. Read access still follows the open-access rule — that is a
/// deliberate deployment mode — but there is nothing to authorise here until an
/// admin exists.
pub async fn require_admin_auth(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    request: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if state.admin_password_hash.lock().is_none() {
        tracing::warn!(
            ip_prefix = %anonymize_ip(&extract_client_ip_from(&state, &headers)),
            "Admin endpoint refused: no admin password is configured yet. \
             Complete POST /api/auth/setup first."
        );
        return Err(StatusCode::UNAUTHORIZED);
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

/// Client IP for a request, honouring the deployment's proxy configuration.
fn extract_client_ip_from(state: &AppState, headers: &HeaderMap) -> String {
    crate::ingest::handler::client_ip(state, headers, None)
}

/// Extract session token from cookie header.
fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get("cookie")?.to_str().ok()?;
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some(token) = part.strip_prefix("mm_session=")
            && !token.is_empty()
        {
            return Some(token.to_string());
        }
    }
    None
}

/// Build a Set-Cookie header value for a session token.
///
/// The `secure` flag should be `true` whenever the server is reachable only
/// over HTTPS (set via `MALLARD_SECURE_COOKIES=true` or inferred from
/// `dashboard_origin` starting with `https://`).
fn build_session_cookie(token: &str, ttl_secs: u64, secure: bool) -> String {
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
        let store = ApiKeyStore::default();
        let key = generate_api_key();
        store.add_key("test-key", &key, ApiKeyScope::ReadOnly);
        assert_eq!(store.validate_key(&key), Some(ApiKeyScope::ReadOnly));
    }

    #[test]
    fn test_api_key_store_invalid_key() {
        let store = ApiKeyStore::default();
        assert!(store.validate_key("invalid-key").is_none());
    }

    #[test]
    fn test_api_key_store_revoke() {
        let store = ApiKeyStore::default();
        let key = generate_api_key();
        let key_hash = store.add_key("test-key", &key, ApiKeyScope::Admin);
        assert!(store.validate_key(&key).is_some());
        store.revoke_key(&key_hash);
        assert!(store.validate_key(&key).is_none());
    }

    #[test]
    fn test_api_key_store_scope_distinction() {
        let store = ApiKeyStore::default();
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
        let store = ApiKeyStore::default();
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
    fn test_session_cookie_includes_secure_when_flag_is_true() {
        let cookie = build_session_cookie("token123", 3600, true);
        assert!(
            cookie.contains("; Secure"),
            "Cookie should include Secure flag when secure=true"
        );
    }

    #[test]
    fn test_session_cookie_omits_secure_when_flag_is_false() {
        let cookie = build_session_cookie("token123", 3600, false);
        assert!(
            !cookie.contains("; Secure"),
            "Cookie must NOT include Secure flag when secure=false"
        );
    }

    // ApiKeyStore cleanup_revoked tests
    #[test]
    fn test_api_key_store_cleanup_revoked() {
        let store = ApiKeyStore::default();
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

    #[test]
    fn test_api_key_store_persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_keys.json");

        // Write two keys to disk via the persisting store.
        let store1 = ApiKeyStore::load_from_disk(path.clone());
        let key_a = generate_api_key();
        let key_b = generate_api_key();
        store1.add_key("alpha", &key_a, ApiKeyScope::ReadOnly);
        store1.add_key("beta", &key_b, ApiKeyScope::Admin);
        assert!(
            path.exists(),
            "api_keys.json should be created after add_key"
        );

        // Load a fresh store from the same file — keys must be present.
        let store2 = ApiKeyStore::load_from_disk(path.clone());
        assert_eq!(
            store2.validate_key(&key_a),
            Some(ApiKeyScope::ReadOnly),
            "key_a must survive a round-trip through disk"
        );
        assert_eq!(
            store2.validate_key(&key_b),
            Some(ApiKeyScope::Admin),
            "key_b must survive a round-trip through disk"
        );

        // Revoke one key and reload — revoked key must not validate.
        let hash_a = hash_api_key(&key_a);
        store2.revoke_key(&hash_a);
        let store3 = ApiKeyStore::load_from_disk(path);
        assert!(
            store3.validate_key(&key_a).is_none(),
            "revoked key must not be valid after reload"
        );
    }

    #[test]
    fn test_secure_cookies_flag_overrides_http_origin() {
        // Even with an HTTP dashboard_origin, secure_cookies=true forces Secure
        // on the cookie, matching the behaviour needed behind a TLS proxy.
        let cookie = build_session_cookie("tok", 3600, true);
        assert!(cookie.contains("; Secure"));
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

    #[test]
    fn test_csrf_validate_referer_authority_match_allowed() {
        // A Referer from the correct host (with a path) must be accepted.
        let mut headers = HeaderMap::new();
        headers.insert(
            "referer",
            "https://analytics.example.com/dashboard/settings"
                .parse()
                .unwrap(),
        );
        assert!(validate_csrf_origin(
            &headers,
            Some(&"https://analytics.example.com".to_string())
        ));
    }

    #[test]
    fn test_csrf_validate_referer_authority_bypass_rejected() {
        // "starts_with" would incorrectly allow this; authority extraction rejects it.
        let mut headers = HeaderMap::new();
        headers.insert(
            "referer",
            "https://analytics.example.com.evil.com/attack"
                .parse()
                .unwrap(),
        );
        assert!(!validate_csrf_origin(
            &headers,
            Some(&"https://analytics.example.com".to_string())
        ));
    }

    #[test]
    fn test_extract_origin_from_referer_https() {
        assert_eq!(
            extract_origin_from_referer("https://example.com/path/page"),
            Some("https://example.com")
        );
    }

    #[test]
    fn test_extract_origin_from_referer_with_port() {
        assert_eq!(
            extract_origin_from_referer("http://localhost:3000/dashboard"),
            Some("http://localhost:3000")
        );
    }

    #[test]
    fn test_extract_origin_from_referer_no_path() {
        assert_eq!(
            extract_origin_from_referer("https://example.com"),
            Some("https://example.com")
        );
    }

    #[test]
    fn test_extract_origin_from_referer_non_http_returns_none() {
        assert_eq!(extract_origin_from_referer("ftp://example.com/file"), None);
    }

    // X-API-Key / X-Forwarded-For helper tests
    #[test]
    fn test_client_ip_helper_respects_the_proxy_setting() {
        // Detailed coverage lives in ingest::handler; this checks the auth-side
        // wrapper reaches the same decision.
        let mut headers = HeaderMap::new();
        headers.insert("x-forwarded-for", "203.0.113.1".parse().unwrap());

        let untrusting = crate::test_support::state_builder().build_state();
        assert_eq!(
            extract_client_ip_from(&untrusting, &headers),
            "unknown",
            "a spoofable header must be ignored when no proxy is configured"
        );

        let trusting = crate::test_support::state_builder()
            .trust_proxy_headers(true)
            .build_state();
        assert_eq!(extract_client_ip_from(&trusting, &headers), "203.0.113.1");
    }

    #[test]
    fn test_anonymize_ip_v4() {
        assert_eq!(anonymize_ip("1.2.3.4"), "1.2.3.x");
        assert_eq!(anonymize_ip("192.168.1.100"), "192.168.1.x");
    }

    #[test]
    fn test_anonymize_ip_v6() {
        // Regression: the old textual split reproduced compressed addresses
        // verbatim, so nothing was actually redacted.
        assert_eq!(anonymize_ip("2001:db8::1"), "2001:db8:0::x");
        assert_eq!(
            anonymize_ip("2001:0db8:85a3:0000:0000:8a2e:0370:7334"),
            "2001:db8:85a3::x"
        );
        for full in ["2001:db8::1", "2001:0db8:85a3:0000:0000:8a2e:0370:7334"] {
            let redacted = anonymize_ip(full);
            assert!(
                !redacted.contains("8a2e") && !redacted.ends_with(":1"),
                "host bits survived redaction: {redacted}"
            );
        }
    }

    #[test]
    fn test_anonymize_ip_rejects_unparseable_input() {
        // Never echo an arbitrary header value into the log line.
        assert_eq!(anonymize_ip("unknown"), "(unparseable)");
        assert_eq!(anonymize_ip("1.2.3"), "(unparseable)");
        assert_eq!(anonymize_ip("\n INJECTED"), "(unparseable)");
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
