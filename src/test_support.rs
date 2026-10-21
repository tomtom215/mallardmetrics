//! Shared test fixtures for building an [`AppState`].
//!
//! `AppState` has thirty-odd fields, and the previous suite built it by hand in
//! five separate places. Adding one field meant editing all five, and a test
//! could not vary a single setting without restating every other one. This
//! builder gives one definition with named overrides.

use crate::api::auth::{ApiKeyStore, LoginAttemptTracker, SessionStore};
use crate::api::stats::QueryLimits;
use crate::ingest::buffer::EventBuffer;
use crate::ingest::geoip::GeoIpReader;
use crate::ingest::handler::AppState;
use crate::ingest::ratelimit::RateLimiter;
use crate::query::cache::QueryCache;
use crate::query::extension::ExtensionState;
use crate::storage::ReaderPool;
use crate::storage::parquet::ParquetStorage;
use duckdb::Connection;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

/// Start building a test [`AppState`].
pub fn state_builder() -> AppStateBuilder {
    AppStateBuilder::default()
}

/// Builder for a test [`AppState`], with sensible defaults for every field.
///
/// The flag count mirrors `AppState`; grouping them into sub-structs here would
/// only make the call sites in tests longer.
#[allow(clippy::struct_excessive_bools)]
pub struct AppStateBuilder {
    allowed_sites: Vec<String>,
    filter_bots: bool,
    trust_proxy_headers: bool,
    trusted_proxy_hops: usize,
    rate_limit_per_site: u32,
    rate_limit_per_ip: u32,
    admin_password_hash: Option<String>,
    dashboard_origin: Option<String>,
    metrics_token: Option<String>,
    max_login_attempts: u32,
    login_lockout_secs: u64,
    query_permits: usize,
    query_limits: QueryLimits,
    cache_ttl_secs: u64,
    secure_cookies: bool,
    behavioral_extension_loaded: bool,
    strip_referrer_query: bool,
    round_timestamps: bool,
    suppress_visitor_id: bool,
    suppress_browser_version: bool,
    suppress_os_version: bool,
    suppress_screen_size: bool,
    geoip_precision: String,
    visitor_salt_rotation_days: u32,
    api_keys: ApiKeyStore,
}

impl Default for AppStateBuilder {
    fn default() -> Self {
        Self {
            allowed_sites: Vec::new(),
            filter_bots: false,
            trust_proxy_headers: false,
            trusted_proxy_hops: crate::ingest::handler::DEFAULT_TRUSTED_PROXY_HOPS,
            rate_limit_per_site: 0,
            rate_limit_per_ip: 0,
            admin_password_hash: None,
            dashboard_origin: None,
            metrics_token: None,
            max_login_attempts: 0,
            login_lockout_secs: 300,
            query_permits: 10,
            query_limits: QueryLimits::DEFAULT,
            cache_ttl_secs: 0,
            secure_cookies: false,
            behavioral_extension_loaded: false,
            strip_referrer_query: false,
            round_timestamps: false,
            suppress_visitor_id: false,
            suppress_browser_version: false,
            suppress_os_version: false,
            suppress_screen_size: false,
            geoip_precision: "city".to_string(),
            visitor_salt_rotation_days: 1,
            api_keys: ApiKeyStore::default(),
        }
    }
}

macro_rules! setter {
    ($name:ident, $ty:ty) => {
        // The macro covers both `Copy` and owned types; the owned ones assign
        // over a value that must be dropped, which `const fn` does not permit.
        #[allow(clippy::missing_const_for_fn)]
        #[must_use]
        pub fn $name(mut self, value: $ty) -> Self {
            self.$name = value;
            self
        }
    };
}

impl AppStateBuilder {
    setter!(allowed_sites, Vec<String>);
    setter!(filter_bots, bool);
    setter!(trust_proxy_headers, bool);
    setter!(trusted_proxy_hops, usize);
    setter!(rate_limit_per_site, u32);
    setter!(rate_limit_per_ip, u32);
    setter!(admin_password_hash, Option<String>);
    setter!(dashboard_origin, Option<String>);
    setter!(metrics_token, Option<String>);
    setter!(query_permits, usize);
    setter!(query_limits, QueryLimits);
    setter!(cache_ttl_secs, u64);
    setter!(secure_cookies, bool);
    setter!(behavioral_extension_loaded, bool);
    setter!(strip_referrer_query, bool);
    setter!(round_timestamps, bool);
    setter!(suppress_visitor_id, bool);
    setter!(suppress_browser_version, bool);
    setter!(suppress_os_version, bool);
    setter!(suppress_screen_size, bool);
    setter!(visitor_salt_rotation_days, u32);
    setter!(api_keys, ApiKeyStore);

    #[allow(clippy::missing_const_for_fn)]
    #[must_use]
    pub fn geoip_precision(mut self, value: &str) -> Self {
        self.geoip_precision = value.to_string();
        self
    }

    /// Enable brute-force protection with the given thresholds.
    #[must_use]
    pub const fn login_limits(mut self, max_attempts: u32, lockout_secs: u64) -> Self {
        self.max_login_attempts = max_attempts;
        self.login_lockout_secs = lockout_secs;
        self
    }

    /// Set the admin password, hashing it as the real setup path would.
    #[must_use]
    pub fn admin_password(mut self, password: &str) -> Self {
        self.admin_password_hash =
            Some(crate::api::auth::hash_password(password).expect("hash the test admin password"));
        self
    }

    /// Build the state along with the temporary directory backing its storage.
    ///
    /// The directory must be kept alive for as long as the state is used.
    pub fn build(self) -> (Arc<AppState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        crate::storage::schema::init_schema(&conn).expect("init schema");
        crate::storage::schema::setup_query_view(&conn, dir.path()).expect("query view");

        let behavioral_loaded = crate::storage::schema::load_behavioral_extension(&conn).is_ok();
        let behavioral_version = crate::storage::schema::behavioral_version(&conn);

        let conn = Arc::new(Mutex::new(conn));
        // An in-memory database is private to its connection, so readers share
        // the writer rather than cloning into an empty catalog.
        let readers = ReaderPool::shared(&conn);
        // Tests that assert on behavioral endpoints opt in explicitly;
        // otherwise the real extension state is used so an environment with
        // the extension available exercises the real code path.
        let behavioral = ExtensionState::new(
            self.behavioral_extension_loaded || behavioral_loaded,
            behavioral_version,
        );
        let state = self.assemble(conn, readers, dir.path(), behavioral);
        (state, dir)
    }

    /// Build the state over a database and reader pool the test made itself,
    /// for tests that need real, independent reader connections (L28) or
    /// control over which connections have the behavioral extension.
    ///
    /// Loads nothing: the extension flag starts false unless
    /// [`AppStateBuilder::behavioral_extension_loaded`] was set.
    pub fn build_over(
        self,
        writer: Arc<Mutex<Connection>>,
        readers: ReaderPool,
        events_dir: &std::path::Path,
    ) -> Arc<AppState> {
        let behavioral = ExtensionState::new(self.behavioral_extension_loaded, None);
        self.assemble(writer, readers, events_dir, behavioral)
    }

    fn assemble(
        self,
        conn: Arc<Mutex<Connection>>,
        readers: ReaderPool,
        events_dir: &std::path::Path,
        behavioral: ExtensionState,
    ) -> Arc<AppState> {
        let storage = ParquetStorage::new(events_dir, 0);
        let tier_lock = crate::storage::TierLock::new();
        let buffer = EventBuffer::new(1000, 0, conn, storage, tier_lock.clone());

        Arc::new(AppState {
            buffer,
            readers,
            tier_lock,
            secret: "test-secret".to_string(),
            allowed_sites: self.allowed_sites,
            geoip: GeoIpReader::open(None),
            filter_bots: self.filter_bots,
            sessions: SessionStore::new(3600),
            api_keys: self.api_keys,
            admin_password: crate::api::auth::AdminPasswordStore::in_memory(
                self.admin_password_hash,
            ),
            dashboard_origin: self.dashboard_origin,
            query_cache: QueryCache::new(self.cache_ttl_secs, 100),
            rate_limiter: RateLimiter::new(self.rate_limit_per_site, 1000),
            ip_rate_limiter: RateLimiter::new(self.rate_limit_per_ip, 1000),
            login_attempt_tracker: LoginAttemptTracker::new(
                self.max_login_attempts,
                self.login_lockout_secs,
            ),
            http_metrics: Arc::default(),
            events_ingested_total: Arc::new(AtomicU64::new(0)),
            flush_failures_total: Arc::new(AtomicU64::new(0)),
            last_flush_success_unix: Arc::new(AtomicU64::new(0)),
            started_at_unix: 0,
            rate_limit_rejections_total: Arc::new(AtomicU64::new(0)),
            login_failures_total: Arc::new(AtomicU64::new(0)),
            alert_metrics: Arc::default(),
            metrics_token: self.metrics_token,
            query_semaphore: Arc::new(tokio::sync::Semaphore::new(self.query_permits)),
            query_limits: self.query_limits,
            secure_cookies: self.secure_cookies,
            behavioral,
            trust_proxy_headers: self.trust_proxy_headers,
            trusted_proxy_hops: self.trusted_proxy_hops,
            session_window: "30 minutes".to_string(),
            realtime_window_minutes: 5,
            visitor_salt_rotation_days: self.visitor_salt_rotation_days,
            strip_referrer_query: self.strip_referrer_query,
            round_timestamps: self.round_timestamps,
            suppress_visitor_id: self.suppress_visitor_id,
            suppress_browser_version: self.suppress_browser_version,
            suppress_os_version: self.suppress_os_version,
            suppress_screen_size: self.suppress_screen_size,
            geoip_precision: self.geoip_precision,
            events_dir: events_dir.to_path_buf(),
            // Inside the fixture's own directory: a sibling of it would be shared
            // by every test (and left behind in the system temp directory).
            data_dir: events_dir.to_path_buf(),
            backup_dir: events_dir.join("backups"),
        })
    }

    /// Build only the state, leaking the temporary directory.
    ///
    /// For unit tests that never touch the filesystem; the directory is cleaned
    /// up when the test process exits.
    pub fn build_state(self) -> Arc<AppState> {
        let (state, dir) = self.build();
        std::mem::forget(dir);
        state
    }
}

/// Whether the behavioral extension is available in this environment.
///
/// Integration tests use this the same way the query-layer tests do: skip
/// locally when the extension cannot be downloaded, fail in CI where
/// `MALLARD_REQUIRE_BEHAVIORAL=1` is set.
pub fn require_behavioral(state: &AppState, what: &str) -> bool {
    if state.behavioral.is_loaded() {
        return true;
    }
    assert!(
        std::env::var("MALLARD_REQUIRE_BEHAVIORAL").as_deref() != Ok("1"),
        "the behavioral extension is required for {what} but could not be loaded, \
         and MALLARD_REQUIRE_BEHAVIORAL=1 is set"
    );
    eprintln!("skipping {what}: behavioral extension unavailable");
    false
}
