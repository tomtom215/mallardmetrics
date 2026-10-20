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

use crate::ingest::ratelimit::network_key;

/// Per-client login-attempt tracker for brute-force protection.
///
/// The map is keyed by client network (see [`network_key`]: an IPv4 address or
/// an IPv6 /64), so it is bounded: without a cap an attacker rotating source
/// addresses grows it without limit. The previous `cleanup` retained every
/// entry with `fail_count > 0` — which is every entry it ever created — so it
/// freed nothing at all.
///
/// Attempts are counted when they are *admitted* ([`begin_attempt`]), not when
/// they fail. Counting afterwards left a gap for the whole of password
/// verification: a burst of parallel requests all passed the check before any
/// failure was recorded, and a client that disconnected mid-verification was
/// never counted although the work still ran.
///
/// [`begin_attempt`]: LoginAttemptTracker::begin_attempt
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

/// Longest lockout applied when the configured one would overflow `Instant`.
const MAX_LOCKOUT: Duration = Duration::from_hours(10 * 365 * 24);

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

    /// Whether the client may attempt a login. `false` means it is locked out.
    ///
    /// Advisory: a request that passes still has to win [`begin_attempt`].
    ///
    /// [`begin_attempt`]: Self::begin_attempt
    pub fn check(&self, ip: &str) -> bool {
        if self.max_attempts == 0 {
            return true;
        }
        let key = network_key(ip);
        let mut map = self.attempts.lock();
        let Some(entry) = map.get_mut(key.as_ref()) else {
            return true;
        };
        let Some(until) = entry.lockout_until else {
            return true;
        };
        if Instant::now() < until {
            return false;
        }
        // The lockout has expired — reset so the client gets a fresh allowance.
        entry.fail_count = 0;
        entry.lockout_until = None;
        entry.last_seen = Instant::now();
        true
    }

    /// Admit one attempt and count it, atomically.
    ///
    /// Returns the attempt number (1-based), or `None` when the client is locked
    /// out. The attempt counts as a failure from this moment; a successful login
    /// calls [`record_success`](Self::record_success), which clears it. The
    /// attempt that reaches `max_attempts` starts the lockout, so exactly
    /// `max_attempts` guesses are ever admitted per window however many requests
    /// arrive at once.
    pub fn begin_attempt(&self, ip: &str) -> Option<u32> {
        if self.max_attempts == 0 {
            return Some(0);
        }
        let key = network_key(ip);
        let now = Instant::now();
        let mut map = self.attempts.lock();
        if let Some(entry) = map.get_mut(key.as_ref()) {
            if let Some(until) = entry.lockout_until {
                if now < until {
                    return None;
                }
                entry.fail_count = 0;
                entry.lockout_until = None;
            }
            return Some(self.count(entry, now, ip));
        }
        if map.len() >= self.max_entries {
            self.make_room(&mut map, now);
        }
        let mut entry = LoginAttemptEntry {
            fail_count: 0,
            lockout_until: None,
            last_seen: now,
        };
        let attempt = self.count(&mut entry, now, ip);
        map.insert(key.into_owned(), entry);
        Some(attempt)
    }

    /// Give back an attempt that was admitted but never judged, because the
    /// server could not run the check (see [`Argon2Busy`]). The client did not
    /// get a guess, so it must not lose one.
    pub fn refund_attempt(&self, ip: &str) {
        if self.max_attempts == 0 {
            return;
        }
        let key = network_key(ip);
        let mut map = self.attempts.lock();
        if let Some(entry) = map.get_mut(key.as_ref()) {
            entry.fail_count = entry.fail_count.saturating_sub(1);
            if entry.fail_count < self.max_attempts {
                entry.lockout_until = None;
            }
        }
    }

    /// Count one attempt on `entry`, starting the lockout when it is the last.
    fn count(&self, entry: &mut LoginAttemptEntry, now: Instant, ip: &str) -> u32 {
        entry.fail_count = entry.fail_count.saturating_add(1);
        entry.last_seen = now;
        if entry.fail_count >= self.max_attempts {
            let lockout = Duration::from_secs(self.lockout_secs).min(MAX_LOCKOUT);
            entry.lockout_until = Some(now + lockout);
            tracing::warn!(
                ip_prefix = %anonymize_ip(ip),
                fail_count = entry.fail_count,
                lockout_secs = lockout.as_secs(),
                "Login brute-force lockout applied"
            );
        }
        entry.fail_count
    }

    /// Record a failed login. Returns the failure count after recording.
    pub fn record_failure(&self, ip: &str) -> u32 {
        if self.max_attempts == 0 {
            return 0;
        }
        let key = network_key(ip);
        let now = Instant::now();
        let mut map = self.attempts.lock();
        if !map.contains_key(key.as_ref()) && map.len() >= self.max_entries {
            self.make_room(&mut map, now);
        }
        let entry = map
            .entry(key.into_owned())
            .or_insert_with(|| LoginAttemptEntry {
                fail_count: 0,
                lockout_until: None,
                last_seen: now,
            });
        self.count(entry, now, ip)
    }

    /// Clear a client's failure history after a successful login.
    pub fn record_success(&self, ip: &str) {
        if self.max_attempts == 0 {
            return;
        }
        self.attempts.lock().remove(network_key(ip).as_ref());
    }

    /// Remaining lockout in seconds, or `None` when not locked out.
    pub fn remaining_lockout_secs(&self, ip: &str) -> Option<u64> {
        if self.max_attempts == 0 {
            return None;
        }
        let key = network_key(ip);
        let map = self.attempts.lock();
        map.get(key.as_ref()).and_then(|entry| {
            entry.lockout_until.and_then(|until| {
                let now = Instant::now();
                (until > now).then(|| until.saturating_duration_since(now).as_secs().max(1))
            })
        })
    }

    /// Free at least an eighth of the table so a new client can be tracked.
    ///
    /// Expired records go first, then the least recently seen un-locked clients,
    /// and only then the locks closest to expiry. Refusing to track a new client
    /// was the alternative, and it let 10,000 failures from distinct sources
    /// switch the protection off for everyone else. The eighth is a batch: the
    /// scan is paid once per batch, not once per attacker request, under the lock
    /// every login needs.
    fn make_room(&self, map: &mut HashMap<String, LoginAttemptEntry>, now: Instant) {
        let target = (self.max_entries / 8).max(1);
        let before = map.len();
        Self::evict_stale(map, now);
        let freed = before - map.len();
        if freed >= target {
            return;
        }
        let need = (target - freed).min(map.len());
        if need == 0 {
            return;
        }
        // Un-locked entries rank below locked ones; within a group, older first.
        let rank = |e: &LoginAttemptEntry| match e.lockout_until {
            Some(until) if until > now => (true, until),
            _ => (false, e.last_seen),
        };
        let mut ranks: Vec<(bool, Instant)> = map.values().map(rank).collect();
        let (_, cutoff, _) = ranks.select_nth_unstable(need - 1);
        let cutoff = *cutoff;
        map.retain(|_, e| rank(e) > cutoff);
        tracing::warn!(
            max_entries = self.max_entries,
            "Login attempt tracker is at capacity; dropped the oldest records"
        );
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

    /// Number of tracked clients (for metrics and tests).
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

/// Most Argon2 operations allowed to run at once.
///
/// Each one holds the algorithm's working memory (about 19 MiB at the crate
/// defaults) and a full core for 50-100 ms. Bounded only by the blocking pool,
/// 512 concurrent logins would pin roughly 10 GiB, and a container memory limit
/// turns that into an OOM kill that also discards the in-memory event buffer.
pub const MAX_CONCURRENT_ARGON2: usize = 4;

/// How long a request waits for an Argon2 slot before giving up.
const ARGON2_QUEUE_WAIT: Duration = Duration::from_secs(10);

static ARGON2_PERMITS: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_ARGON2)));

/// The server is already running as many password operations as it allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Argon2Busy;

/// Wait up to `wait` for a slot in `permits`.
async fn acquire_within(
    permits: &Arc<tokio::sync::Semaphore>,
    wait: Duration,
) -> Result<tokio::sync::OwnedSemaphorePermit, Argon2Busy> {
    tokio::time::timeout(wait, Arc::clone(permits).acquire_owned())
        .await
        .ok()
        .and_then(Result::ok)
        .ok_or(Argon2Busy)
}

/// Wait (briefly) for an Argon2 slot.
async fn acquire_argon2_permit() -> Result<tokio::sync::OwnedSemaphorePermit, Argon2Busy> {
    acquire_within(&ARGON2_PERMITS, ARGON2_QUEUE_WAIT).await
}

/// Hash a password on the blocking thread pool.
///
/// Argon2 is deliberately CPU-expensive. Running it directly on an async worker
/// pins that worker for the duration, so a handful of concurrent login attempts
/// could stall every other request on the server. At most
/// [`MAX_CONCURRENT_ARGON2`] run at once; a request that cannot get a slot in
/// time fails like any other hashing error.
///
/// # Errors
///
/// Returns an error if hashing fails, the server is saturated, or the blocking
/// task panics.
pub async fn hash_password_async(password: String) -> Result<String, argon2::password_hash::Error> {
    let permit = acquire_argon2_permit()
        .await
        .map_err(|Argon2Busy| argon2::password_hash::Error::Crypto)?;
    tokio::task::spawn_blocking(move || {
        let hashed = hash_password(&password);
        drop(permit);
        hashed
    })
    .await
    .unwrap_or(Err(argon2::password_hash::Error::Crypto))
}

/// Verify a password on the blocking thread pool, within the Argon2 limit.
///
/// The slot is released by the blocking task itself, not by this future: a
/// client that disconnects drops the future, but the hashing it started still
/// runs to completion and must keep counting against the limit until it does.
///
/// # Errors
///
/// Returns [`Argon2Busy`] when no slot became free in time. The password was not
/// checked, so the caller must not treat it as a wrong guess.
pub async fn verify_password_limited(password: String, hash: String) -> Result<bool, Argon2Busy> {
    let permit = acquire_argon2_permit().await?;
    Ok(tokio::task::spawn_blocking(move || {
        let ok = verify_password(&password, &hash);
        drop(permit);
        ok
    })
    .await
    .unwrap_or(false))
}

/// Verify a password on the blocking thread pool. See [`hash_password_async`].
///
/// A saturated server answers `false`; use [`verify_password_limited`] to tell
/// that apart from a wrong password.
pub async fn verify_password_async(password: String, hash: String) -> bool {
    verify_password_limited(password, hash)
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

/// Constant-time byte-slice comparison, for secrets.
///
/// Delegates to `subtle`, which is written specifically to survive an
/// optimising compiler. The previous hand-rolled fold was constant-time only as
/// long as LLVM chose not to turn it into an early-exit loop — nothing in the
/// language stops it, and the whole value of the function is that guarantee.
///
/// Length is compared first and is not itself secret here: every caller
/// compares fixed-width values (a hex digest, or a token of known length).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
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

/// Longest session lifetime applied when the configured one overflows `Instant`.
const MAX_SESSION_TTL: Duration = Duration::from_hours(10 * 365 * 24);

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
                // A TTL too large for `Instant` is "never expires in practice";
                // adding it unchecked panicked on a config value.
                expires_at: now.checked_add(self.ttl).unwrap_or(now + MAX_SESSION_TTL),
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

/// Distinguishes the temporary files of concurrent writers in one process.
static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Write `contents` to `path` atomically, readable only by the owner.
///
/// Used for both `api_keys.json` and the visitor-ID secret. Both were
/// previously written with `std::fs::write`, which truncates first (so a crash
/// mid-write loses the file) and inherits the process umask (so on a typical
/// system they landed world-readable).
///
/// Each call writes its own temporary file (process ID plus a sequence number).
/// A name shared between calls let two concurrent writers truncate and
/// interleave into one file, and rename the garbled result into place.
///
/// # Errors
///
/// Returns an error if the file cannot be written, its permissions cannot be
/// set, the rename into place fails, or the directory entry cannot be flushed.
pub fn write_private_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(dir)?;

    // Same directory as the target, so the rename below stays within one
    // filesystem and is therefore atomic.
    let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp_path = path.with_extension(format!("tmp.{}.{sequence}", std::process::id()));

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
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
    })?;

    // The rename is only durable once the directory entry is. Directories
    // cannot be opened for this on every platform, so it is a unix step.
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// Remove leftovers of interrupted [`write_private_file`] calls next to `path`.
///
/// A crash between creating the temporary file and renaming it leaves a 0600
/// copy of the secret or key file that nothing ever deleted. Call this at
/// startup, before anything is written. Returns the number of files removed.
pub fn remove_stale_temp_files(path: &std::path::Path) -> usize {
    let (Some(dir), Some(name)) = (path.parent(), path.file_stem().and_then(|n| n.to_str())) else {
        return 0;
    };
    let prefix = format!("{name}.tmp.");
    let Ok(entries) = std::fs::read_dir(if dir.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        dir
    }) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
        .filter(|entry| std::fs::remove_file(entry.path()).is_ok())
        .count()
}

/// Restrict an existing secret file to its owner.
///
/// [`write_private_file`] creates files 0600, but one written by an older
/// version, or restored from a backup, keeps whatever mode it had. Returns
/// whether the mode had to be changed.
pub fn restrict_to_owner(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(metadata) = std::fs::metadata(path) else {
            return false;
        };
        // Any group or other permission bit set: 0o077 reads better than a
        // trailing-zeros count for a file mode.
        #[allow(clippy::verbose_bit_mask)]
        let private = metadata.permissions().mode() & 0o077 == 0;
        if private {
            return false;
        }
        let changed =
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).is_ok();
        if changed {
            tracing::warn!(path = %path.display(), "Tightened a secret file's permissions to 0600");
        }
        changed
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

/// The admin password hash, with optional disk persistence.
///
/// The hash used to live in a bare `Mutex<Option<String>>` that nothing ever
/// wrote to disk. A password set through `POST /api/auth/setup` therefore
/// survived only until the process stopped: on the next start `setup_required`
/// was true again and the first request to reach the instance — from anyone —
/// could claim it. Persisting the hash closes that window.
///
/// `MALLARD_ADMIN_PASSWORD` still wins when it is set, because an operator who
/// supplies the password on every boot expects that value to be authoritative.
#[derive(Clone, Default)]
pub struct AdminPasswordStore {
    hash: Arc<Mutex<Option<String>>>,
    /// Where a password set through the setup endpoint is written. `None`
    /// disables persistence, which is what tests and env-supplied passwords use.
    persist_path: Option<Arc<std::path::PathBuf>>,
}

/// On-disk form of the admin credential.
#[derive(Serialize, Deserialize)]
struct StoredAdminPassword {
    /// Argon2id PHC string. Never the password itself.
    password_hash: String,
    updated_at: chrono::NaiveDateTime,
}

impl AdminPasswordStore {
    /// A store that keeps the hash in memory only.
    pub fn in_memory(hash: Option<String>) -> Self {
        Self {
            hash: Arc::new(Mutex::new(hash)),
            persist_path: None,
        }
    }

    /// A store backed by `path`.
    ///
    /// `env_hash` is the hash of `MALLARD_ADMIN_PASSWORD`, when one was
    /// supplied. It takes precedence over the file and is not written back:
    /// the environment already carries it on every start, and writing it would
    /// leave a stale credential behind after the variable was removed.
    pub fn load_from_disk(path: std::path::PathBuf, env_hash: Option<String>) -> Self {
        if let Some(hash) = env_hash {
            return Self {
                hash: Arc::new(Mutex::new(Some(hash))),
                persist_path: Some(Arc::new(path)),
            };
        }

        remove_stale_temp_files(&path);
        restrict_to_owner(&path);
        let stored = match std::fs::read_to_string(&path) {
            Ok(contents) => match serde_json::from_str::<StoredAdminPassword>(&contents) {
                Ok(record) if PasswordHash::new(&record.password_hash).is_ok() => {
                    tracing::info!(path = %path.display(), "Loaded the admin password hash");
                    Some(record.password_hash)
                }
                Ok(_) => {
                    tracing::error!(
                        path = %path.display(),
                        "The stored admin password hash is not a valid Argon2 PHC string; \
                         refusing to use it. Delete the file to run setup again, or set \
                         MALLARD_ADMIN_PASSWORD."
                    );
                    None
                }
                Err(e) => {
                    tracing::error!(
                        path = %path.display(),
                        error = %e,
                        "Could not parse the stored admin password; the dashboard will \
                         ask for setup again. Move the file aside or set \
                         MALLARD_ADMIN_PASSWORD."
                    );
                    None
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                tracing::error!(
                    path = %path.display(),
                    error = %e,
                    "Could not read the stored admin password"
                );
                None
            }
        };

        Self {
            hash: Arc::new(Mutex::new(stored)),
            persist_path: Some(Arc::new(path)),
        }
    }

    /// The current hash, if one is configured.
    pub fn get(&self) -> Option<String> {
        self.hash.lock().clone()
    }

    /// Whether an admin password exists.
    pub fn is_configured(&self) -> bool {
        self.hash.lock().is_some()
    }

    /// Install `hash` if and only if none is configured yet.
    ///
    /// Returns `false` when a password was already set, so two concurrent setup
    /// requests cannot both succeed. Persisting happens under the same lock: a
    /// failure to write is reported to the caller rather than leaving a password
    /// that works until the next restart and then silently reopens setup.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the hash could not be persisted.
    pub fn set_if_unset(&self, hash: String) -> Result<bool, std::io::Error> {
        let mut guard = self.hash.lock();
        if guard.is_some() {
            return Ok(false);
        }
        if let Some(path) = &self.persist_path {
            let record = StoredAdminPassword {
                password_hash: hash.clone(),
                updated_at: chrono::Utc::now().naive_utc(),
            };
            let json = serde_json::to_vec_pretty(&record).map_err(std::io::Error::other)?;
            write_private_file(path.as_ref(), &json)?;
            tracing::info!(path = %path.display(), "Persisted the admin password hash (mode 0600)");
        }
        *guard = Some(hash);
        Ok(true)
    }
}

/// Thread-safe API key store with optional disk persistence.
///
/// When `persist_path` is set, any mutation (`try_add_key`, `try_revoke_key`) is
/// written to disk as a JSON array of `StoredApiKey` records before it is
/// reported as done. On startup the caller should use
/// `ApiKeyStore::load_from_disk` to restore keys written by previous runs.
#[derive(Clone)]
pub struct ApiKeyStore {
    keys: Arc<Mutex<Vec<StoredApiKey>>>,
    persist_path: Option<Arc<std::path::PathBuf>>,
    /// Held while the key list is snapshotted *and* written, so the last write
    /// to reach the disk is always the newest state. Snapshotting under one lock
    /// and writing after releasing it let an older snapshot be renamed into
    /// place last, which silently undid a revocation.
    persist_lock: Arc<Mutex<()>>,
}

impl Default for ApiKeyStore {
    fn default() -> Self {
        Self {
            keys: Arc::new(Mutex::new(Vec::new())),
            persist_path: None,
            persist_lock: Arc::new(Mutex::new(())),
        }
    }
}

impl ApiKeyStore {
    /// Create a store that loads existing keys from `path` and persists
    /// mutations back to the same file.  Missing file is treated as empty.
    ///
    /// A file that cannot be parsed is moved aside (`api_keys.json.corrupt-<time>`)
    /// rather than left in place: the store starts empty, and the next mutation
    /// would otherwise overwrite the only copy of every key.
    pub fn load_from_disk(path: std::path::PathBuf) -> Self {
        remove_stale_temp_files(&path);
        restrict_to_owner(&path);
        let keys = match std::fs::read_to_string(&path) {
            Ok(contents) => {
                serde_json::from_str::<Vec<StoredApiKey>>(&contents).unwrap_or_else(|e| {
                    let aside = path.with_extension(format!(
                        "json.corrupt-{}",
                        chrono::Utc::now().timestamp()
                    ));
                    match std::fs::rename(&path, &aside) {
                        Ok(()) => tracing::error!(
                            path = %path.display(),
                            moved_to = %aside.display(),
                            error = %e,
                            "Failed to parse api_keys.json; moved it aside and starting with an empty key store"
                        ),
                        Err(rename_error) => tracing::error!(
                            path = %path.display(),
                            error = %e,
                            rename_error = %rename_error,
                            "Failed to parse api_keys.json and could not move it aside; starting with an empty key store"
                        ),
                    }
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
            persist_lock: Arc::new(Mutex::new(())),
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
    /// Blocks on file I/O, so async callers must use `spawn_blocking`.
    fn persist(&self) -> std::io::Result<()> {
        let Some(path) = &self.persist_path else {
            return Ok(());
        };
        let _serialised = self.persist_lock.lock();
        let snapshot = self.keys.lock().clone();
        let json = serde_json::to_vec_pretty(&snapshot).map_err(std::io::Error::other)?;
        write_private_file(path.as_ref(), &json)
    }

    /// Store a new API key (hashed) and persist it. Returns the key hash.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the key could not be written to disk. The key
    /// is then not stored at all: answering "created" for a key that vanishes
    /// on the next restart is worse than failing.
    pub fn try_add_key(
        &self,
        name: &str,
        plaintext_key: &str,
        scope: ApiKeyScope,
    ) -> std::io::Result<String> {
        let key_hash = hash_api_key(plaintext_key);
        let stored = StoredApiKey {
            key_hash: key_hash.clone(),
            name: name.to_string(),
            scope,
            created_at: chrono::Utc::now().naive_utc(),
            revoked: false,
        };
        self.keys.lock().push(stored);
        if let Err(e) = self.persist() {
            self.keys.lock().retain(|k| k.key_hash != key_hash);
            return Err(e);
        }
        Ok(key_hash)
    }

    /// Store a new API key (hashed). Returns the key hash for identification.
    ///
    /// A persistence failure is logged and the key stays in memory only; use
    /// [`try_add_key`](Self::try_add_key) where the caller can report it.
    pub fn add_key(&self, name: &str, plaintext_key: &str, scope: ApiKeyScope) -> String {
        self.try_add_key(name, plaintext_key, scope)
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "Failed to persist API keys to disk");
                let key_hash = hash_api_key(plaintext_key);
                self.keys.lock().push(StoredApiKey {
                    key_hash: key_hash.clone(),
                    name: name.to_string(),
                    scope,
                    created_at: chrono::Utc::now().naive_utc(),
                    revoked: false,
                });
                key_hash
            })
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

    /// Revoke an API key by hash and persist the revocation.
    ///
    /// Returns `Ok(false)` when no such key exists.
    ///
    /// # Errors
    ///
    /// Returns the I/O error if the revocation could not be written. The key is
    /// then left valid: reporting "revoked" for a key that is live again after
    /// a restart is the dangerous direction to fail in.
    pub fn try_revoke_key(&self, key_hash: &str) -> std::io::Result<bool> {
        let was_live = {
            let mut keys = self.keys.lock();
            let Some(key) = keys.iter_mut().find(|k| k.key_hash == key_hash) else {
                return Ok(false);
            };
            let was_live = !key.revoked;
            key.revoked = true;
            was_live
        };
        if let Err(e) = self.persist() {
            if was_live
                && let Some(key) = self.keys.lock().iter_mut().find(|k| k.key_hash == key_hash)
            {
                key.revoked = false;
            }
            return Err(e);
        }
        Ok(true)
    }

    /// Revoke an API key by hash. A persistence failure is logged; the key is
    /// revoked in memory regardless. Prefer [`try_revoke_key`](Self::try_revoke_key).
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
        if found && let Err(e) = self.persist() {
            tracing::warn!(error = %e, "Failed to persist API keys to disk");
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
///
/// Public so that every route a password enters by (the setup endpoint and
/// `MALLARD_ADMIN_PASSWORD` at startup) applies the same policy.
///
/// # Errors
///
/// Returns the message explaining why the password was refused.
pub fn validate_password(password: &str) -> Result<(), String> {
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
fn session_response(state: &AppState, token: &str) -> Response {
    let secure = state.secure_cookies
        || state
            .dashboard_origin
            .as_deref()
            .is_some_and(|o| o.starts_with("https://"));
    let cookie = build_session_cookie(token, state.sessions.ttl_secs(), secure);
    // The token travels only in the HttpOnly cookie. Repeating it in the body
    // handed the credential to any script or log that could see the response,
    // which is exactly what HttpOnly exists to prevent.
    (
        StatusCode::OK,
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({"status": "ok"})),
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
    crate::ingest::handler::PeerAddr(peer): crate::ingest::handler::PeerAddr,
    headers: HeaderMap,
    Json(body): Json<PasswordRequest>,
) -> impl IntoResponse {
    let ip = extract_client_ip_from(&state, &headers, peer);

    if !csrf_ok(&state, &headers) {
        return csrf_refused();
    }

    // Counted on the way in: a rejected attempt costs the caller one try, and
    // an attempt that never finishes (the client hung up) still cost one.
    if state.login_attempt_tracker.begin_attempt(&ip).is_none() {
        return too_many_attempts(&state, &ip);
    }

    if let Err(message) = validate_password(&body.password) {
        // Setup is unauthenticated, so without counting rejections an attacker
        // could probe it without limit.
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message })),
        )
            .into_response();
    }

    // Check-then-hash, re-checking after: hashing takes ~50-100 ms and must not
    // hold the lock, but two concurrent setup requests must not both succeed.
    if state.admin_password.is_configured() {
        state.login_attempt_tracker.refund_attempt(&ip);
        return already_configured();
    }

    let hash = match hash_password_async(body.password).await {
        Ok(h) => h,
        Err(e) => {
            state.login_attempt_tracker.refund_attempt(&ip);
            tracing::error!(error = %e, "Failed to hash password during setup");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Internal error"})),
            )
                .into_response();
        }
    };

    let store = state.admin_password.clone();
    let persisted = tokio::task::spawn_blocking(move || store.set_if_unset(hash))
        .await
        .unwrap_or_else(|e| Err(std::io::Error::other(e)));
    match persisted {
        Ok(true) => {}
        Ok(false) => {
            state.login_attempt_tracker.refund_attempt(&ip);
            return already_configured();
        }
        Err(e) => {
            // Accepting the password without storing it would leave the
            // instance claimable again after the next restart, which is exactly
            // the failure this endpoint exists to prevent.
            tracing::error!(error = %e, "Could not persist the admin password");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "Could not store the admin password. Check that the data \
                              directory is writable, then try again."
                })),
            )
                .into_response();
        }
    }

    state.login_attempt_tracker.record_success(&ip);
    tracing::info!("Admin password configured via the setup endpoint");

    session_response(&state, &state.sessions.create_session("admin"))
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
    crate::ingest::handler::PeerAddr(peer): crate::ingest::handler::PeerAddr,
    headers: HeaderMap,
    Json(body): Json<PasswordRequest>,
) -> impl IntoResponse {
    let ip = extract_client_ip_from(&state, &headers, peer);

    if !csrf_ok(&state, &headers) {
        return csrf_refused();
    }

    // Cheap early refusal for a client that is already locked out.
    if !state.login_attempt_tracker.check(&ip) {
        return too_many_attempts(&state, &ip);
    }

    // Clone the stored hash and release the lock immediately. Verification is
    // ~50-100 ms of Argon2 work; holding the mutex across it serialised every
    // login attempt and blocked every other reader of the hash, and running it
    // inline pinned an async worker thread for the duration.
    let stored_hash = state.admin_password.get();
    let Some(stored_hash) = stored_hash else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No admin password configured. Use /api/auth/setup first."
            })),
        )
            .into_response();
    };

    // The attempt is counted *before* the password is checked. Counting after
    // let a burst of parallel requests all pass the lockout check before any
    // failure was recorded, and let a client that disconnected mid-check keep
    // its guess for free.
    let Some(attempt) = state.login_attempt_tracker.begin_attempt(&ip) else {
        return too_many_attempts(&state, &ip);
    };

    match verify_password_limited(body.password, stored_hash).await {
        Err(Argon2Busy) => {
            // The password was never checked, so the client keeps its attempt.
            state.login_attempt_tracker.refund_attempt(&ip);
            return busy_response();
        }
        Ok(false) => {
            state
                .login_failures_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                ip_prefix = %anonymize_ip(&ip),
                fail_count = attempt,
                "Admin login failed: invalid password"
            );
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "Invalid password"})),
            )
                .into_response();
        }
        Ok(true) => {}
    }

    state.login_attempt_tracker.record_success(&ip);
    tracing::info!(ip_prefix = %anonymize_ip(&ip), "Admin login successful");

    session_response(&state, &state.sessions.create_session("admin"))
}

/// 503 for a request the server could not run a password check for.
fn busy_response() -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": "Server is busy. Try again shortly."})),
    )
        .into_response();
    response
        .headers_mut()
        .insert("retry-after", axum::http::HeaderValue::from_static("1"));
    response
}

/// Whether a login, setup or logout request passes the origin check.
///
/// These endpoints are not behind [`require_admin_auth`], so the CSRF check
/// that guards the other state-changing routes has to be applied here too:
/// a cross-site login request can bind a victim's browser to an attacker's
/// session. A request with neither `Origin` nor `Referer` (curl, a script) is
/// still allowed, as everywhere else.
fn csrf_ok(state: &AppState, headers: &HeaderMap) -> bool {
    validate_csrf_origin(headers, state.dashboard_origin.as_ref())
}

fn csrf_refused() -> Response {
    tracing::warn!("CSRF check failed on an authentication endpoint");
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({"error": "Cross-origin request refused"})),
    )
        .into_response()
}

/// POST /api/auth/logout — Invalidate the current session.
pub async fn auth_logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if !csrf_ok(&state, &headers) {
        return csrf_refused();
    }

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
        .into_response()
}

/// GET /api/auth/status — Check authentication state.
///
/// Returns whether setup is needed and whether the current request is authenticated.
pub async fn auth_status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let setup_required = !state.admin_password.is_configured();
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
) -> Response {
    if body.name.is_empty() || body.name.len() > 128 {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Key name must be 1-128 characters"})),
        )
            .into_response();
    }

    let plaintext_key = generate_api_key();
    // The write is an fsync; it must not run on an async worker (L19).
    let store = state.api_keys.clone();
    let (name, scope, key) = (body.name.clone(), body.scope, plaintext_key.clone());
    let key_hash = match tokio::task::spawn_blocking(move || store.try_add_key(&name, &key, scope))
        .await
    {
        Ok(Ok(hash)) => hash,
        Ok(Err(e)) => return persistence_failed("create the API key", &e),
        Err(e) => {
            tracing::error!(error = %e, "API key persistence task panicked");
            return persistence_failed("create the API key", &std::io::Error::other("task failed"));
        }
    };

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
) -> Result<Response, crate::api::errors::ApiError> {
    let store = state.api_keys.clone();
    let target = key_hash.clone();
    match tokio::task::spawn_blocking(move || store.try_revoke_key(&target)).await {
        Ok(Ok(true)) => {
            tracing::info!(key_hash_prefix = %key_hash.get(..8).unwrap_or(&key_hash), "API key revoked");
            Ok((
                StatusCode::OK,
                Json(serde_json::json!({"status": "revoked"})),
            )
                .into_response())
        }
        Ok(Ok(false)) => Err(crate::api::errors::ApiError::NotFound(
            "Key not found".to_string(),
        )),
        Ok(Err(e)) => Ok(persistence_failed("revoke the API key", &e)),
        Err(e) => {
            tracing::error!(error = %e, "API key persistence task panicked");
            Ok(persistence_failed(
                "revoke the API key",
                &std::io::Error::other("task failed"),
            ))
        }
    }
}

/// 500 for a key-store change that could not be written to disk.
///
/// The change has been rolled back, so the response and the on-disk state agree.
fn persistence_failed(action: &str, error: &std::io::Error) -> Response {
    tracing::error!(error = %error, action, "Could not persist the API key store");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": format!(
                "Could not {action}: the key store is not writable. Check that the data \
                 directory is writable, then try again."
            )
        })),
    )
        .into_response()
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

/// Canonical form of an origin for comparison: lower-case, no default port.
///
/// Browsers send `Origin` lower-cased and without `:443`/`:80`, so a configured
/// `https://Dashboard.example.com:443` never matched byte for byte and every
/// session-authenticated admin call was refused. Scheme and host are
/// case-insensitive (RFC 3986), and a default port names the same origin.
fn normalise_origin(origin: &str) -> String {
    let lowered = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    lowered
        .strip_suffix(":443")
        .filter(|rest| rest.starts_with("https://"))
        .or_else(|| {
            lowered
                .strip_suffix(":80")
                .filter(|rest| rest.starts_with("http://"))
        })
        .map_or_else(|| lowered.clone(), str::to_string)
}

/// Validate that the request Origin or Referer matches the configured dashboard origin.
///
/// This prevents CSRF attacks on session-authenticated state-changing endpoints.
/// Only enforced when `dashboard_origin` is configured.
fn validate_csrf_origin(headers: &HeaderMap, dashboard_origin: Option<&String>) -> bool {
    let Some(expected) = dashboard_origin else {
        return true; // No restriction configured
    };

    let expected = normalise_origin(expected);

    if let Some(origin) = headers.get("origin") {
        if let Ok(origin_str) = origin.to_str() {
            return normalise_origin(origin_str) == expected;
        }
        return false;
    }

    if let Some(referer) = headers.get("referer") {
        if let Ok(referer_str) = referer.to_str() {
            // Extract only the scheme+authority from the Referer URL before comparing.
            // Using starts_with() would allow "https://example.com.evil.com/…" to bypass
            // a rule for "https://example.com".
            let referer_origin = extract_origin_from_referer(referer_str).unwrap_or("");
            return normalise_origin(referer_origin) == expected;
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
    if !state.admin_password.is_configured() {
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
    if !state.admin_password.is_configured() {
        let peer = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|axum::extract::ConnectInfo(addr)| *addr);
        tracing::warn!(
            ip_prefix = %anonymize_ip(&extract_client_ip_from(&state, &headers, peer)),
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
///
/// `peer` is the socket address the request arrived on. It used to be passed as
/// `None`, which meant that on any deployment not behind a trusted proxy —
/// which is the default — every login attempt was keyed by the literal string
/// `"unknown"`. Brute-force protection was therefore global rather than
/// per-IP: five failures from anybody locked *everyone* out of the dashboard
/// for the lockout period, and an attacker gained nothing by rotating source
/// addresses because they were never distinguished in the first place.
fn extract_client_ip_from(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> String {
    crate::ingest::handler::client_ip(state, headers, peer)
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

    // ── Admin password persistence ───────────────────────────────────────

    #[test]
    fn test_admin_password_survives_a_restart() {
        // Before this, a password set through /api/auth/setup lived only in
        // memory: after a restart the instance reported setup_required again
        // and the next request to reach it — from anyone — could claim it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.json");

        let first = AdminPasswordStore::load_from_disk(path.clone(), None);
        assert!(!first.is_configured(), "a fresh instance has no password");
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(first.set_if_unset(hash).unwrap());

        let after_restart = AdminPasswordStore::load_from_disk(path, None);
        assert!(after_restart.is_configured(), "the password must survive");
        assert!(verify_password(
            "correct horse battery staple",
            &after_restart.get().unwrap()
        ));
    }

    #[test]
    fn test_admin_password_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.json");
        let store = AdminPasswordStore::load_from_disk(path.clone(), None);
        store
            .set_if_unset(hash_password("a-long-enough-password").unwrap())
            .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "the hash must not be world-readable");
        }
        // The password itself must never reach the file.
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("a-long-enough-password"));
        assert!(contents.contains("$argon2"));
    }

    #[test]
    fn test_set_if_unset_refuses_a_second_password() {
        let store = AdminPasswordStore::in_memory(None);
        assert!(store.set_if_unset("first".to_string()).unwrap());
        assert!(
            !store.set_if_unset("second".to_string()).unwrap(),
            "two concurrent setup requests must not both win"
        );
        assert_eq!(store.get().as_deref(), Some("first"));
    }

    #[test]
    fn test_env_password_wins_over_the_stored_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.json");
        AdminPasswordStore::load_from_disk(path.clone(), None)
            .set_if_unset(hash_password("stored-password-x").unwrap())
            .unwrap();

        let env_hash = hash_password("env-password-value").unwrap();
        let store = AdminPasswordStore::load_from_disk(path, Some(env_hash));
        let live = store.get().unwrap();
        assert!(verify_password("env-password-value", &live));
        assert!(!verify_password("stored-password-x", &live));
    }

    #[test]
    fn test_a_corrupt_stored_hash_is_not_trusted() {
        // A truncated or hand-edited file must not become an admin credential
        // that Argon2 then fails open on.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("admin.json");
        std::fs::write(
            &path,
            r#"{"password_hash":"not-a-phc-string","updated_at":"2024-01-01T00:00:00"}"#,
        )
        .unwrap();
        let store = AdminPasswordStore::load_from_disk(path, None);
        assert!(!store.is_configured());
    }

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

    #[test]
    fn test_login_tracker_still_tracks_new_ips_when_full() {
        // SI-05 regression: with the table full of ordinary one-failure records
        // the tracker refused to track anyone new, and `check()` allows an
        // untracked address, so after 10,000 failures from distinct sources
        // every further source could guess without limit.
        let tracker = LoginAttemptTracker::with_capacity(3, 300, 4);
        for i in 0..4 {
            tracker.record_failure(&format!("198.51.100.{i}"));
        }
        for _ in 0..3 {
            tracker.record_failure("203.0.113.99");
        }
        assert!(
            !tracker.check("203.0.113.99"),
            "an address that keeps failing must lock out even when the table was full"
        );
        assert!(tracker.tracked_ips() <= 4);
    }

    #[test]
    fn test_login_lockout_covers_the_whole_ipv6_64() {
        // SI-04 regression: one /64 is one subscriber. Keyed on the full
        // address, each guess from a fresh /128 started a new allowance.
        let tracker = LoginAttemptTracker::new(3, 300);
        tracker.record_failure("2001:db8:1:2::1");
        tracker.record_failure("2001:db8:1:2::2");
        tracker.record_failure("2001:db8:1:2:abcd::3");
        assert!(!tracker.check("2001:db8:1:2::ffff"));
        assert!(
            tracker.check("2001:db8:1:3::1"),
            "a neighbouring /64 is unaffected"
        );
    }

    #[test]
    fn test_begin_attempt_admits_exactly_the_allowed_number() {
        let tracker = LoginAttemptTracker::new(3, 300);
        assert_eq!(tracker.begin_attempt("192.0.2.1"), Some(1));
        assert_eq!(tracker.begin_attempt("192.0.2.1"), Some(2));
        assert_eq!(tracker.begin_attempt("192.0.2.1"), Some(3));
        assert_eq!(
            tracker.begin_attempt("192.0.2.1"),
            None,
            "a fourth guess is refused before any password is checked"
        );
        assert_eq!(tracker.begin_attempt("192.0.2.2"), Some(1));
    }

    #[test]
    fn test_begin_attempt_is_atomic_under_contention() {
        // SI-06: with check-then-record, N threads all passed `check` before any
        // of them recorded. One lock acquisition must decide each.
        let tracker = LoginAttemptTracker::new(5, 300);
        let admitted = std::sync::atomic::AtomicU32::new(0);
        std::thread::scope(|scope| {
            for _ in 0..32 {
                scope.spawn(|| {
                    if tracker.begin_attempt("198.51.100.7").is_some() {
                        admitted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                });
            }
        });
        assert_eq!(admitted.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    #[test]
    fn test_success_after_begin_attempt_clears_the_count_and_lockout() {
        let tracker = LoginAttemptTracker::new(2, 300);
        tracker.begin_attempt("192.0.2.9");
        // The last allowed attempt starts the lockout as it is admitted...
        assert_eq!(tracker.begin_attempt("192.0.2.9"), Some(2));
        assert!(!tracker.check("192.0.2.9"));
        // ...and a correct password on it still clears everything.
        tracker.record_success("192.0.2.9");
        assert!(tracker.check("192.0.2.9"));
        assert_eq!(tracker.begin_attempt("192.0.2.9"), Some(1));
    }

    #[test]
    fn test_refund_returns_an_attempt_the_client_never_got_to_use() {
        let tracker = LoginAttemptTracker::new(2, 300);
        tracker.begin_attempt("192.0.2.5");
        assert_eq!(tracker.begin_attempt("192.0.2.5"), Some(2));
        assert!(!tracker.check("192.0.2.5"), "locked by the second attempt");
        tracker.refund_attempt("192.0.2.5");
        assert!(tracker.check("192.0.2.5"), "the refund lifts that lockout");
        assert_eq!(tracker.begin_attempt("192.0.2.5"), Some(2));
    }

    #[test]
    fn test_full_table_keeps_active_lockouts_and_drops_idle_records() {
        let tracker = LoginAttemptTracker::with_capacity(2, 300, 8);
        tracker.record_failure("203.0.113.1");
        tracker.record_failure("203.0.113.1");
        for i in 0..7 {
            tracker.record_failure(&format!("198.51.100.{i}"));
        }
        // Eight tracked, one of them locked. A newcomer must still be tracked,
        // at the expense of an un-locked record.
        tracker.record_failure("192.0.2.200");
        assert!(
            !tracker.check("203.0.113.1"),
            "the lockout survives pressure"
        );
        assert!(tracker.attempts.lock().contains_key("192.0.2.200"));
        assert!(tracker.tracked_ips() <= 8);
    }

    #[test]
    fn test_table_pressure_is_relieved_in_batches() {
        let tracker = LoginAttemptTracker::with_capacity(5, 300, 80);
        for i in 0..80 {
            tracker.record_failure(&format!("10.0.{}.{}", i / 250, i % 250));
        }
        tracker.record_failure("192.0.2.1");
        assert!(
            tracker.tracked_ips() <= 80 - 10 + 1,
            "an eighth of the table was freed, not one slot ({} left)",
            tracker.tracked_ips()
        );
    }

    #[test]
    fn test_enormous_lockout_does_not_panic() {
        // SI-20: `Instant::now() + Duration::from_secs(u64::MAX)` panics, and
        // `record_failure` runs this on the fifth failed login from the network.
        let tracker = LoginAttemptTracker::new(1, u64::MAX);
        tracker.record_failure("192.0.2.77");
        assert!(!tracker.check("192.0.2.77"));
        assert!(tracker.remaining_lockout_secs("192.0.2.77").is_some());
    }

    #[test]
    fn test_enormous_session_ttl_does_not_panic() {
        let sessions = SessionStore::new(u64::MAX);
        let token = sessions.create_session("admin");
        assert!(sessions.validate_session(&token).is_some());
    }

    #[tokio::test]
    async fn test_argon2_slots_are_bounded() {
        // SI-06: with no cap, 512 concurrent logins held ~10 GiB of Argon2
        // memory. A caller that cannot get a slot in time is turned away.
        let permits = Arc::new(tokio::sync::Semaphore::new(2));
        let a = acquire_within(&permits, Duration::from_millis(50))
            .await
            .unwrap();
        let b = acquire_within(&permits, Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(
            acquire_within(&permits, Duration::from_millis(50))
                .await
                .unwrap_err(),
            Argon2Busy
        );
        drop(a);
        assert!(
            acquire_within(&permits, Duration::from_millis(50))
                .await
                .is_ok()
        );
        drop(b);
    }

    #[tokio::test]
    async fn test_verify_password_limited_checks_the_password() {
        let hash = hash_password("a-correct-password").unwrap();
        assert_eq!(
            verify_password_limited("a-correct-password".into(), hash.clone()).await,
            Ok(true)
        );
        assert_eq!(
            verify_password_limited("a-wrong-password".into(), hash).await,
            Ok(false)
        );
    }

    #[tokio::test]
    async fn test_a_login_abandoned_mid_flight_still_costs_an_attempt() {
        // SI-06 path C: the guess is counted when it is admitted, so hanging up
        // before the answer arrives cannot make it free. Every Argon2 slot is
        // held so the request is certainly still in flight when it is dropped.
        let state = crate::test_support::state_builder()
            .admin_password("correct-horse-battery")
            .login_limits(1, 300)
            .build_state();
        let held = ARGON2_PERMITS
            .clone()
            .acquire_many_owned(u32::try_from(MAX_CONCURRENT_ARGON2).unwrap())
            .await
            .unwrap();

        let attempt = auth_login(
            State(Arc::clone(&state)),
            crate::ingest::handler::PeerAddr(None),
            HeaderMap::new(),
            Json(PasswordRequest {
                password: "a guess".to_string(),
            }),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(150), attempt)
                .await
                .is_err(),
            "the request must still be waiting for a slot"
        );
        drop(held);

        assert!(
            !state.login_attempt_tracker.check("unknown"),
            "the abandoned guess used up the only attempt"
        );
    }

    #[test]
    fn test_origin_comparison_ignores_case_and_default_ports() {
        let expected = "https://Dashboard.Example.com:443".to_string();
        let mut headers = HeaderMap::new();
        headers.insert("origin", "https://dashboard.example.com".parse().unwrap());
        assert!(validate_csrf_origin(&headers, Some(&expected)));
        headers.insert(
            "origin",
            "https://dashboard.example.com.evil.com".parse().unwrap(),
        );
        assert!(!validate_csrf_origin(&headers, Some(&expected)));
        headers.insert("origin", "http://dashboard.example.com".parse().unwrap());
        assert!(
            !validate_csrf_origin(&headers, Some(&expected)),
            "scheme matters"
        );
        headers.insert(
            "origin",
            "https://dashboard.example.com:8443".parse().unwrap(),
        );
        assert!(
            !validate_csrf_origin(&headers, Some(&expected)),
            "port matters"
        );
    }

    // API key persistence (SI-16)

    #[test]
    fn test_concurrent_key_changes_all_reach_the_disk_intact() {
        // Snapshots were taken under one lock and written after releasing it, to
        // a temp file whose name was shared, so concurrent mutations could
        // rename an older snapshot last or interleave into a garbled file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_keys.json");
        let store = ApiKeyStore::load_from_disk(path.clone());
        std::thread::scope(|scope| {
            for i in 0..16 {
                let store = &store;
                scope.spawn(move || {
                    store
                        .try_add_key(
                            &format!("key-{i}"),
                            &format!("mm_secret_{i}"),
                            ApiKeyScope::ReadOnly,
                        )
                        .unwrap();
                });
            }
        });
        let on_disk: Vec<StoredApiKey> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.len(), 16, "the last write holds every key");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp files are renamed or removed");
    }

    #[test]
    fn test_a_revocation_is_not_undone_by_a_racing_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_keys.json");
        let store = ApiKeyStore::load_from_disk(path.clone());
        let hash = store
            .try_add_key("victim", "mm_victim", ApiKeyScope::Admin)
            .unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| assert!(store.try_revoke_key(&hash).unwrap()));
            for i in 0..8 {
                let store = &store;
                scope.spawn(move || {
                    store
                        .try_add_key(
                            &format!("other-{i}"),
                            &format!("mm_other_{i}"),
                            ApiKeyScope::ReadOnly,
                        )
                        .unwrap();
                });
            }
        });
        let reloaded = ApiKeyStore::load_from_disk(path);
        assert_eq!(
            reloaded.validate_key("mm_victim"),
            None,
            "still revoked after a restart"
        );
        assert_eq!(reloaded.list_keys().len(), 9);
    }

    #[test]
    fn test_a_failed_write_is_reported_and_rolled_back() {
        // The parent of the key file is a regular file, so every write fails
        // whatever user the tests run as.
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"x").unwrap();
        let store = ApiKeyStore::load_from_disk(blocker.join("api_keys.json"));

        assert!(
            store
                .try_add_key("lost", "mm_lost", ApiKeyScope::Admin)
                .is_err()
        );
        assert_eq!(
            store.validate_key("mm_lost"),
            None,
            "a key that could not be stored must not authenticate"
        );
        assert!(store.list_keys().is_empty());

        // Revocation fails the other way round: the key must stay as it was.
        let memory_only = ApiKeyStore::default();
        let hash = memory_only
            .try_add_key("live", "mm_live", ApiKeyScope::Admin)
            .unwrap();
        assert!(!memory_only.try_revoke_key("no-such-hash").unwrap());
        assert!(memory_only.try_revoke_key(&hash).unwrap());
        assert_eq!(memory_only.validate_key("mm_live"), None);
    }

    #[test]
    fn test_a_failed_revocation_leaves_the_key_valid_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"x").unwrap();
        let store = ApiKeyStore {
            keys: Arc::new(Mutex::new(vec![StoredApiKey {
                key_hash: hash_api_key("mm_k"),
                name: "k".to_string(),
                scope: ApiKeyScope::Admin,
                created_at: chrono::Utc::now().naive_utc(),
                revoked: false,
            }])),
            persist_path: Some(Arc::new(blocker.join("api_keys.json"))),
            persist_lock: Arc::new(Mutex::new(())),
        };
        assert!(store.try_revoke_key(&hash_api_key("mm_k")).is_err());
        assert_eq!(
            store.validate_key("mm_k"),
            Some(ApiKeyScope::Admin),
            "a revocation that did not persist must not be reported as done"
        );
    }

    #[test]
    fn test_an_unreadable_key_file_is_kept_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_keys.json");
        std::fs::write(&path, b"{ this is not json").unwrap();

        let store = ApiKeyStore::load_from_disk(path.clone());
        assert!(store.list_keys().is_empty());
        store
            .try_add_key("fresh", "mm_fresh", ApiKeyScope::ReadOnly)
            .unwrap();

        let kept: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(kept.len(), 1, "the unparseable file was moved aside");
        assert_eq!(
            std::fs::read(kept[0].path()).unwrap(),
            b"{ this is not json",
            "and its contents are intact"
        );
        assert!(
            ApiKeyStore::load_from_disk(path)
                .validate_key("mm_fresh")
                .is_some()
        );
    }

    #[test]
    fn test_concurrent_private_writes_never_interleave() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        std::thread::scope(|scope| {
            for i in 0..8u8 {
                let path = &path;
                scope.spawn(move || {
                    let payload = vec![b'a' + i; 64 * 1024];
                    write_private_file(path, &payload).unwrap();
                });
            }
        });
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 64 * 1024);
        assert!(
            bytes.iter().all(|b| *b == bytes[0]),
            "one writer's content, whole"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_existing_secret_files_are_restricted_on_load() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_keys.json");
        std::fs::write(&path, b"[]").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let _ = ApiKeyStore::load_from_disk(path.clone());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a file written by an older version is tightened"
        );
        assert!(!restrict_to_owner(&path), "already private: nothing to do");
    }

    #[test]
    fn test_stale_temp_files_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("api_keys.json");
        std::fs::write(dir.path().join("api_keys.tmp.4242.0"), b"secret").unwrap();
        std::fs::write(dir.path().join("api_keys.tmp.4242"), b"secret").unwrap();
        std::fs::write(dir.path().join("unrelated.txt"), b"keep").unwrap();
        assert_eq!(remove_stale_temp_files(&path), 2);
        assert!(dir.path().join("unrelated.txt").exists());

        // The same sweep covers the visitor-ID secret, which has no extension.
        let secret = dir.path().join(".secret");
        std::fs::write(dir.path().join(".secret.tmp.1.0"), b"s").unwrap();
        assert_eq!(remove_stale_temp_files(&secret), 1);
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
            extract_client_ip_from(&untrusting, &headers, None),
            "unknown",
            "a spoofable header must be ignored when no proxy is configured"
        );

        let trusting = crate::test_support::state_builder()
            .trust_proxy_headers(true)
            .build_state();
        assert_eq!(
            extract_client_ip_from(&trusting, &headers, None),
            "203.0.113.1"
        );
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
