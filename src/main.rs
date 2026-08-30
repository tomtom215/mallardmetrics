//! Mallard Metrics server entry point.
//!
//! The binary drives the library crate rather than re-declaring the module
//! tree, so everything is compiled once instead of twice.

use mallard_metrics::api::auth::{
    ApiKeyStore, LoginAttemptTracker, SessionStore, write_private_file,
};
use mallard_metrics::config::Config;
use mallard_metrics::ingest::buffer::EventBuffer;
use mallard_metrics::ingest::geoip::GeoIpReader;
use mallard_metrics::ingest::handler::AppState;
use mallard_metrics::ingest::ratelimit::RateLimiter;
use mallard_metrics::query::cache::QueryCache;
use mallard_metrics::storage::ReaderPool;
use mallard_metrics::storage::parquet::ParquetStorage;
use mallard_metrics::{server, storage};

use duckdb::Connection;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// How often the housekeeping task sweeps expired sessions, cache entries and
/// rate-limit buckets.
const HOUSEKEEPING_INTERVAL: std::time::Duration = std::time::Duration::from_mins(15);
/// How often data-retention cleanup runs.
const RETENTION_INTERVAL: std::time::Duration = std::time::Duration::from_hours(24);

/// Exit code used for a configuration or startup failure.
const EXIT_FAILURE: i32 = 1;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--healthcheck") => std::process::exit(run_healthcheck().await),
        Some("--version" | "-V") => {
            println!("mallard-metrics {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Some("--help" | "-h") => {
            print_help();
            return;
        }
        _ => {}
    }

    let config_path = args.first().map(std::path::PathBuf::from);
    run(config_path.as_deref()).await;
}

fn print_help() {
    println!(
        "mallard-metrics {version}\n\
         Self-hosted, privacy-focused web analytics.\n\n\
         USAGE:\n    \
             mallard-metrics [CONFIG_FILE]\n    \
             mallard-metrics --healthcheck\n\n\
         OPTIONS:\n    \
             CONFIG_FILE      Path to a TOML configuration file (optional)\n    \
             --healthcheck    Probe a running instance and exit 0 when healthy\n    \
             --version, -V    Print the version and exit\n    \
             --help, -h       Print this message\n\n\
         Configuration may also be supplied through MALLARD_* environment\n\
         variables; see mallard-metrics.toml.example for the full list.",
        version = env!("CARGO_PKG_VERSION")
    );
}

/// Probe a running instance's readiness endpoint.
///
/// The release image is built `FROM scratch`, so it contains no shell, curl or
/// wget — a container healthcheck has to be the binary itself.
async fn run_healthcheck() -> i32 {
    let Ok(LoadedOrDefault(config)) = load_config_quietly() else {
        eprintln!("healthcheck: configuration is invalid");
        return EXIT_FAILURE;
    };

    // Probe the loopback address rather than the configured bind address, which
    // may be 0.0.0.0.
    let host = if config.host == "0.0.0.0" || config.host == "::" {
        "127.0.0.1"
    } else {
        &config.host
    };
    let url = format!("http://{host}:{}/health/ready", config.port);

    match probe(&url).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("healthcheck: {e}");
            EXIT_FAILURE
        }
    }
}

/// A configuration loaded without emitting warnings.
struct LoadedOrDefault(Config);

fn load_config_quietly() -> Result<LoadedOrDefault, String> {
    let path = std::env::args().nth(1).filter(|a| !a.starts_with("--"));
    let loaded = Config::load(path.as_deref().map(std::path::Path::new))?;
    Ok(LoadedOrDefault(loaded.config))
}

/// Issue a minimal HTTP GET and require a 200 response.
///
/// Hand-rolled rather than pulling in an HTTP client: the only request this
/// binary ever makes as a client is this one, to its own loopback address.
async fn probe(url: &str) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let rest = url
        .strip_prefix("http://")
        .ok_or("only http:// is supported")?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let request = format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");

    let connect = tokio::net::TcpStream::connect(authority);
    let mut stream = tokio::time::timeout(std::time::Duration::from_secs(5), connect)
        .await
        .map_err(|_| "connection timed out".to_string())?
        .map_err(|e| format!("could not connect to {authority}: {e}"))?;

    stream
        .write_all(request.as_bytes())
        .await
        .map_err(|e| format!("write failed: {e}"))?;

    let mut response = Vec::with_capacity(256);
    let read = stream.read_to_end(&mut response);
    tokio::time::timeout(std::time::Duration::from_secs(5), read)
        .await
        .map_err(|_| "read timed out".to_string())?
        .map_err(|e| format!("read failed: {e}"))?;

    let head = String::from_utf8_lossy(&response);
    if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        let status = head.lines().next().unwrap_or("(no response)");
        Err(format!("unhealthy: {status}"))
    }
}

/// Start the server.
async fn run(config_path: Option<&std::path::Path>) {
    // Configuration is resolved before logging is initialised, because the log
    // format is itself a configuration value. Errors here go to stderr.
    let loaded = match Config::load(config_path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            std::process::exit(EXIT_FAILURE);
        }
    };
    let config = loaded.config;

    if let Err(e) = config.validate() {
        eprintln!("Configuration error: {e}");
        std::process::exit(EXIT_FAILURE);
    }

    init_tracing(&config.log_format);

    // Warnings collected during loading, now that a subscriber exists.
    for warning in &loaded.warnings {
        tracing::warn!("{warning}");
    }
    for advisory in config.advisories() {
        tracing::warn!("{advisory}");
    }

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        host = %config.host,
        port = config.port,
        data_dir = %config.data_dir.display(),
        "Starting Mallard Metrics"
    );

    if let Err(e) = std::fs::create_dir_all(config.events_dir()) {
        eprintln!(
            "Failed to create the data directory {}: {e}",
            config.events_dir().display()
        );
        std::process::exit(EXIT_FAILURE);
    }

    let state = match build_state(&config) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("Startup failed: {e}");
            std::process::exit(EXIT_FAILURE);
        }
    };

    spawn_background_tasks(&config, &state);

    let app = server::build_router(Arc::clone(&state));
    let addr = format!("{}:{}", config.host, config.port);
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("Failed to bind to {addr}: {e}");
            std::process::exit(EXIT_FAILURE);
        }
    };

    tracing::info!(addr = %addr, "Listening");

    // ConnectInfo makes the peer address available to handlers, which is what
    // the client-IP fallback uses when no proxy headers are trusted.
    let service = app.into_make_service_with_connect_info::<SocketAddr>();
    let shutdown_timeout = config.shutdown_timeout_secs;

    if let Err(e) = axum::serve(listener, service)
        .with_graceful_shutdown(shutdown_signal(Arc::clone(&state), shutdown_timeout))
        .await
    {
        tracing::error!(error = %e, "Server error");
        std::process::exit(EXIT_FAILURE);
    }
}

/// Initialise the tracing subscriber.
///
/// `log_format` comes from configuration, so a TOML `log_format = "json"` now
/// takes effect. The field existed and was documented, but nothing ever read
/// it: the subscriber was built from the environment variable alone.
fn init_tracing(log_format: &str) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "mallard_metrics=info,tower_http=info".into());

    if log_format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }
}

/// The database handles a running server needs.
struct Database {
    writer: Arc<Mutex<Connection>>,
    readers: ReaderPool,
    behavioral_loaded: bool,
    behavioral_version: Option<String>,
}

/// Open the database, run migrations, load the extension, and build the view.
///
/// The extension is loaded on every connection because `LOAD` arms one
/// connection, not the database. The `events_all` view is a catalog object that
/// all connections share, so it is defined once on the writer — which is why a
/// flush or an erasure only refreshes it there.
fn open_database(config: &Config, storage: &ParquetStorage) -> Result<Database, String> {
    let conn = Connection::open(config.db_path())
        .map_err(|e| format!("could not open {}: {e}", config.db_path().display()))?;
    storage::migrations::run_migrations(&conn).map_err(|e| format!("migrations failed: {e}"))?;

    let behavioral_loaded = match storage::schema::load_behavioral_extension(&conn) {
        Ok(()) => {
            tracing::info!(
                version = storage::schema::behavioral_version(&conn),
                "Behavioral extension loaded"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Behavioral extension unavailable; funnels, retention, sessions, \
                 sequences and flow analysis will report 503 until it loads"
            );
            false
        }
    };
    let behavioral_version = storage::schema::behavioral_version(&conn);

    let events_dir = config.events_dir();

    // Remove `.parquet.tmp` files left by an interrupted write before defining
    // the view, so they never accumulate.
    match storage.cleanup_temp_files() {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "Cleaned up interrupted Parquet writes"),
        Err(e) => tracing::warn!(error = %e, "Could not clean up interrupted Parquet writes"),
    }

    if let Err(e) = storage::schema::setup_query_view(&conn, &events_dir) {
        tracing::warn!(error = %e, "Could not create the events_all view; queries will be limited");
    }

    let writer = Arc::new(Mutex::new(conn));
    let readers = ReaderPool::new(&writer, config.read_connections);
    if readers.len() > 1 {
        // Only the extension needs arming per connection; the view the writer
        // just created is already in the shared catalog.
        let _ = readers.for_each(storage::schema::load_behavioral_extension);
    }

    Ok(Database {
        writer,
        readers,
        behavioral_loaded,
        behavioral_version,
    })
}

/// Assemble the shared application state.
fn build_state(config: &Config) -> Result<Arc<AppState>, String> {
    let events_dir = config.events_dir();
    let storage = ParquetStorage::new(&events_dir, config.compact_after_files);
    let db = open_database(config, &storage)?;

    let buffer = EventBuffer::new(
        config.flush_event_count,
        config.max_buffered_events,
        Arc::clone(&db.writer),
        storage,
    );

    let metrics_token = std::env::var("MALLARD_METRICS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if metrics_token.is_some() {
        tracing::info!("Metrics endpoint protected by MALLARD_METRICS_TOKEN");
    }

    // 0 means "no limit"; the semaphore expresses that as the maximum count.
    let max_concurrent = if config.max_concurrent_queries == 0 {
        usize::MAX
    } else {
        config.max_concurrent_queries
    };

    if config.gdpr_mode {
        tracing::info!(
            geoip_precision = %config.geoip_precision,
            "GDPR mode enabled: referrer queries stripped, timestamps rounded to the hour, \
             browser/OS versions and screen size suppressed"
        );
    }

    Ok(Arc::new(AppState {
        buffer,
        readers: db.readers,
        secret: load_or_create_secret(config),
        allowed_sites: config.site_ids.clone(),
        geoip: GeoIpReader::open(config.geoip_db_path.as_deref()),
        filter_bots: config.filter_bots,
        sessions: SessionStore::with_capacity(config.session_ttl_secs, config.max_sessions),
        api_keys: ApiKeyStore::load_from_disk(config.data_dir.join("api_keys.json")),
        admin_password_hash: Mutex::new(load_admin_password()),
        dashboard_origin: config.dashboard_origin.clone(),
        query_cache: QueryCache::new(config.cache_ttl_secs, config.cache_max_entries),
        rate_limiter: RateLimiter::new(config.rate_limit_per_site, config.max_tracked_keys),
        ip_rate_limiter: RateLimiter::new(config.rate_limit_per_ip, config.max_tracked_keys),
        login_attempt_tracker: LoginAttemptTracker::with_capacity(
            config.max_login_attempts,
            config.login_lockout_secs,
            config.max_tracked_keys,
        ),
        events_ingested_total: Arc::new(AtomicU64::new(0)),
        flush_failures_total: Arc::new(AtomicU64::new(0)),
        rate_limit_rejections_total: Arc::new(AtomicU64::new(0)),
        login_failures_total: Arc::new(AtomicU64::new(0)),
        metrics_token,
        query_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
        secure_cookies: config.secure_cookies,
        behavioral_extension_loaded: db.behavioral_loaded,
        behavioral_version: db.behavioral_version,
        trust_proxy_headers: config.trust_proxy_headers,
        session_window: config.session_window_interval(),
        realtime_window_minutes: config.realtime_window_minutes,
        visitor_salt_rotation_days: config.visitor_salt_rotation_days,
        strip_referrer_query: config.strip_referrer_query,
        round_timestamps: config.round_timestamps,
        suppress_visitor_id: config.suppress_visitor_id,
        suppress_browser_version: config.suppress_browser_version,
        suppress_os_version: config.suppress_os_version,
        suppress_screen_size: config.suppress_screen_size,
        geoip_precision: config.geoip_precision.clone(),
        events_dir,
    }))
}

/// Hash the admin password supplied through the environment, if any.
fn load_admin_password() -> Option<String> {
    let password = std::env::var("MALLARD_ADMIN_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty())?;

    match mallard_metrics::api::auth::hash_password(&password) {
        Ok(hash) => {
            tracing::info!("Admin password configured from MALLARD_ADMIN_PASSWORD");
            Some(hash)
        }
        Err(e) => {
            // Exiting would be worse: it would leave an unauthenticated
            // dashboard running is not an option either, so refuse to start.
            eprintln!("Failed to hash MALLARD_ADMIN_PASSWORD: {e}");
            std::process::exit(EXIT_FAILURE);
        }
    }
}

/// Resolve the visitor-ID secret, generating and persisting one if needed.
///
/// Priority: `MALLARD_SECRET`, then `data_dir/.secret`, then a fresh value.
/// Persisting it prevents the old behaviour where every restart silently
/// generated a new secret and permanently broke visitor deduplication.
///
/// The file is written with owner-only permissions: anyone who can read it can
/// re-derive every visitor ID from an IP and User-Agent, which would undo the
/// pseudonymisation the whole design rests on.
fn load_or_create_secret(config: &Config) -> String {
    if let Ok(secret) = std::env::var("MALLARD_SECRET") {
        let secret = secret.trim().to_string();
        if !secret.is_empty() {
            return secret;
        }
    }

    let secret_path = config.data_dir.join(".secret");
    if let Ok(existing) = std::fs::read_to_string(&secret_path) {
        let existing = existing.trim().to_string();
        if !existing.is_empty() {
            tracing::info!(path = %secret_path.display(), "Loaded the persisted visitor-ID secret");
            return existing;
        }
    }

    let secret = uuid::Uuid::new_v4().to_string();
    match write_private_file(&secret_path, secret.as_bytes()) {
        Ok(()) => tracing::info!(
            path = %secret_path.display(),
            "Generated and persisted a visitor-ID secret (mode 0600). \
             Set MALLARD_SECRET to supply your own."
        ),
        Err(e) => tracing::warn!(
            error = %e,
            path = %secret_path.display(),
            "Could not persist the visitor-ID secret. Visitor IDs will change on \
             the next restart unless MALLARD_SECRET is set."
        ),
    }
    secret
}

/// Spawn the periodic flush, retention and housekeeping tasks.
fn spawn_background_tasks(config: &Config, state: &Arc<AppState>) {
    spawn_flush_task(config.flush_interval_secs, state);
    if config.retention_days > 0 {
        spawn_retention_task(config.retention_days, state);
    }
    spawn_housekeeping_task(state);
}

/// Flush buffered events to Parquet on a timer.
///
/// The flush blocks: it takes a `parking_lot` mutex and writes files. Awaiting
/// the interval is cheap, but the work itself goes to `spawn_blocking` so it
/// never occupies an async worker thread.
fn spawn_flush_task(interval_secs: u64, state: &Arc<AppState>) {
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        // A slow flush must not cause a burst of catch-up ticks afterwards.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let task_state = Arc::clone(&state);
            let result = tokio::task::spawn_blocking(move || task_state.buffer.flush()).await;
            match result {
                Ok(Ok(count)) if count > 0 => tracing::info!(count, "Periodic flush completed"),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    state.flush_failures_total.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(error = %e, "Periodic flush failed");
                }
                Err(e) => {
                    state.flush_failures_total.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(error = %e, "Periodic flush task panicked");
                }
            }
        }
    });
}

/// Delete Parquet partitions past the retention horizon, daily.
fn spawn_retention_task(retention_days: u32, state: &Arc<AppState>) {
    let storage = state.buffer.storage().clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RETENTION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let storage = storage.clone();
            let result =
                tokio::task::spawn_blocking(move || storage.cleanup_old_partitions(retention_days))
                    .await;
            match result {
                Ok(Ok(0)) => {}
                Ok(Ok(removed)) => {
                    tracing::info!(removed, retention_days, "Data retention cleanup completed");
                }
                Ok(Err(e)) => tracing::error!(error = %e, "Data retention cleanup failed"),
                Err(e) => tracing::error!(error = %e, "Data retention cleanup task panicked"),
            }
        }
    });
}

/// Sweep expired sessions, cache entries, rate-limit buckets and revoked keys.
fn spawn_housekeeping_task(state: &Arc<AppState>) {
    let sessions = state.sessions.clone();
    let cache = state.query_cache.clone();
    let site_limiter = state.rate_limiter.clone();
    let ip_limiter = state.ip_rate_limiter.clone();
    let login_tracker = state.login_attempt_tracker.clone();
    let api_keys = state.api_keys.clone();

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HOUSEKEEPING_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            sessions.cleanup_expired();
            cache.cleanup_expired();
            site_limiter.cleanup();
            ip_limiter.cleanup();
            login_tracker.cleanup();
            api_keys.cleanup_revoked();
        }
    });
}

/// Wait for SIGINT or SIGTERM, then flush buffered events.
async fn shutdown_signal(state: Arc<AppState>, timeout_secs: u64) {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!(error = %e, "Failed to install the Ctrl+C handler");
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "Failed to install the SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("Received SIGINT"),
        () = terminate => tracing::info!("Received SIGTERM"),
    }

    tracing::info!(
        timeout_secs,
        buffered = state.buffer.len(),
        "Shutting down gracefully, flushing buffered events"
    );

    let flush = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || state.buffer.flush()
    });

    let timeout = std::time::Duration::from_secs(timeout_secs.max(1));
    match tokio::time::timeout(timeout, flush).await {
        Ok(Ok(Ok(count))) if count > 0 => {
            tracing::info!(count, "Flushed remaining events during shutdown");
        }
        Ok(Ok(Ok(_))) => {}
        Ok(Ok(Err(e))) => tracing::error!(error = %e, "Failed to flush events during shutdown"),
        Ok(Err(e)) => tracing::error!(error = %e, "Flush task panicked during shutdown"),
        Err(_) => tracing::warn!(
            timeout_secs,
            "Graceful shutdown flush timed out; some buffered events may be lost"
        ),
    }

    tracing::info!("Shutdown complete");
}
