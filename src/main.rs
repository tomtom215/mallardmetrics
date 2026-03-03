mod api;
mod config;
mod dashboard;
mod ingest;
mod query;
mod server;
mod storage;

use crate::api::auth::{ApiKeyStore, SessionStore};
use crate::config::Config;
use crate::ingest::buffer::EventBuffer;
use crate::ingest::geoip::GeoIpReader;
use crate::ingest::handler::AppState;
use crate::storage::parquet::ParquetStorage;
use duckdb::Connection;
use parking_lot::Mutex;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Initialize tracing (read log format before config loads so early logs are formatted)
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "mallard_metrics=info,tower_http=info".into());
    let log_format = std::env::var("MALLARD_LOG_FORMAT").unwrap_or_default();
    if log_format == "json" {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    // Load configuration
    let config_path = std::env::args().nth(1);
    let config = Config::load(config_path.as_deref().map(std::path::Path::new));

    // Validate configuration before binding
    if let Err(e) = config.validate() {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    }

    tracing::info!(
        host = %config.host,
        port = config.port,
        data_dir = %config.data_dir.display(),
        "Starting Mallard Metrics"
    );

    // Ensure data directory exists
    std::fs::create_dir_all(config.events_dir()).expect("Failed to create data directory");

    // Initialize DuckDB using a disk-based file so that events buffered in the
    // `events` table (not yet flushed to Parquet) survive a process crash.
    // The WAL file written next to mallard.duckdb provides atomic batch inserts.
    //
    // NOTE: if the server crashes in the narrow window after `COPY TO` succeeds
    // but before `DELETE FROM events` commits, those events may appear in both
    // the DuckDB table and the Parquet file.  The events_all VIEW unions both
    // tiers, so such events would be counted twice.  This is an acceptable
    // trade-off for lightweight analytics; the probability is extremely low.
    let conn = Connection::open(config.db_path()).expect("Failed to open DuckDB");
    storage::migrations::run_migrations(&conn).expect("Failed to run migrations");

    // Try to load the behavioral extension (non-fatal if unavailable)
    let behavioral_extension_loaded = match storage::schema::load_behavioral_extension(&conn) {
        Ok(()) => {
            tracing::info!("Behavioral extension loaded");
            true
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "Behavioral extension not available; behavioral analytics features will be disabled"
            );
            false
        }
    };

    // Create the events_all view that unions the hot events table with persisted
    // Parquet files on disk.  This makes historical data queryable immediately,
    // including data written by previous server runs.  Non-fatal: if no Parquet
    // files exist yet the view falls back to a passthrough over the events table.
    match storage::schema::setup_query_view(&conn, &config.events_dir()) {
        Ok(()) => tracing::info!("Query view initialised"),
        Err(e) => {
            tracing::warn!(error = %e, "Could not create events_all view; queries limited to buffered events");
        }
    }

    let conn = Arc::new(Mutex::new(conn));
    let storage = ParquetStorage::new(&config.events_dir());
    let buffer = EventBuffer::new(config.flush_event_count, Arc::clone(&conn), storage);

    // Initialize GeoIP reader (gracefully degrades if .mmdb not available)
    let geoip = GeoIpReader::open(config.geoip_db_path.as_deref());

    let state = build_app_state(&config, buffer, geoip, behavioral_extension_loaded);

    // Spawn background tasks
    spawn_background_tasks(&config, &conn, &state);

    let app = server::build_router(Arc::clone(&state));
    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind to {addr}: {e}"));

    tracing::info!(addr = %addr, "Listening");

    // Graceful shutdown on SIGINT/SIGTERM
    let shutdown_timeout = config.shutdown_timeout_secs;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(Arc::clone(&state), shutdown_timeout))
        .await
        .expect("Server error");
}

#[allow(clippy::too_many_lines)]
fn build_app_state(
    config: &Config,
    buffer: EventBuffer,
    geoip: GeoIpReader,
    behavioral_extension_loaded: bool,
) -> Arc<AppState> {
    let sessions = SessionStore::new(config.session_ttl_secs);
    // Load API keys from disk so they survive server restarts.  Keys are
    // written back to the same file on every add/revoke operation.
    let api_keys_path = config.data_dir.join("api_keys.json");
    let api_keys = ApiKeyStore::load_from_disk(api_keys_path);
    let query_cache =
        crate::query::cache::QueryCache::new(config.cache_ttl_secs, config.cache_max_entries);
    let rate_limiter = crate::ingest::ratelimit::RateLimiter::new(config.rate_limit_per_site);
    let login_attempt_tracker = crate::api::auth::LoginAttemptTracker::new(
        config.max_login_attempts,
        config.login_lockout_secs,
    );

    let admin_password_hash = std::env::var("MALLARD_ADMIN_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty())
        .map(|p| {
            let hash = crate::api::auth::hash_password(&p).expect("Failed to hash admin password");
            tracing::info!("Admin password configured from MALLARD_ADMIN_PASSWORD");
            hash
        });

    // Load or generate-and-persist the visitor-ID secret.
    //
    // MALLARD_SECRET (env var) takes highest priority.  If unset, we look for a
    // previously-persisted secret at `data_dir/.secret`.  If that file does not
    // exist we generate a fresh UUID, persist it, and emit an INFO log.
    //
    // This prevents the old behaviour where every restart silently generated a
    // new random secret, permanently corrupting historical visitor deduplication.
    let secret = std::env::var("MALLARD_SECRET").unwrap_or_else(|_| {
        let secret_path = config.data_dir.join(".secret");
        if let Ok(s) = std::fs::read_to_string(&secret_path) {
            let s = s.trim().to_string();
            if !s.is_empty() {
                tracing::info!(path = %secret_path.display(), "Loaded persisted MALLARD_SECRET");
                return s;
            }
        }
        let secret = uuid::Uuid::new_v4().to_string();
        match std::fs::write(&secret_path, &secret) {
            Ok(()) => {
                tracing::info!(
                    path = %secret_path.display(),
                    "Generated and persisted MALLARD_SECRET. \
                     Set MALLARD_SECRET env var to use a custom value."
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Could not persist MALLARD_SECRET to disk. \
                     Visitor IDs will change on next restart unless MALLARD_SECRET is set."
                );
            }
        }
        secret
    });

    let metrics_token = std::env::var("MALLARD_METRICS_TOKEN")
        .ok()
        .filter(|t| !t.is_empty());
    if metrics_token.is_some() {
        tracing::info!("Metrics endpoint protected by bearer token (MALLARD_METRICS_TOKEN)");
    }

    // Limit concurrent heavy analytics queries to prevent a tight query loop from
    // monopolising the single DuckDB connection.  0 in config → unlimited.
    let max_concurrent = if config.max_concurrent_queries == 0 {
        usize::MAX
    } else {
        config.max_concurrent_queries
    };

    if config.gdpr_mode {
        tracing::info!(
            "GDPR mode enabled: strip_referrer_query, round_timestamps, \
             suppress_browser_version, suppress_os_version, suppress_screen_size active; \
             geoip_precision={:?}",
            config.geoip_precision
        );
        if config.retention_days == 0 {
            tracing::warn!(
                "GDPR mode is enabled but retention_days is 0 (unlimited). \
                 Consider setting MALLARD_RETENTION_DAYS=30 for GDPR Art. 5(1)(e) storage limitation."
            );
        }
    }

    Arc::new(AppState {
        buffer,
        secret,
        allowed_sites: config.site_ids.clone(),
        geoip,
        filter_bots: config.filter_bots,
        sessions,
        api_keys,
        admin_password_hash: Mutex::new(admin_password_hash),
        dashboard_origin: config.dashboard_origin.clone(),
        query_cache,
        rate_limiter,
        login_attempt_tracker,
        events_ingested_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        flush_failures_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        rate_limit_rejections_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        login_failures_total: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        metrics_token,
        query_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
        secure_cookies: config.secure_cookies,
        behavioral_extension_loaded,
        strip_referrer_query: config.strip_referrer_query,
        round_timestamps: config.round_timestamps,
        suppress_visitor_id: config.suppress_visitor_id,
        suppress_browser_version: config.suppress_browser_version,
        suppress_os_version: config.suppress_os_version,
        suppress_screen_size: config.suppress_screen_size,
        geoip_precision: config.geoip_precision.clone(),
        events_dir: config.events_dir(),
    })
}

fn spawn_background_tasks(config: &Config, conn: &Arc<Mutex<Connection>>, state: &Arc<AppState>) {
    // Periodic flush task.
    //
    // The flush involves blocking operations: parking_lot::Mutex::lock() (futex
    // wait under contention) and DuckDB COPY TO Parquet (filesystem I/O).
    // Running these on a Tokio async worker thread would starve the scheduler.
    // Instead, we await the interval (non-blocking) then hand the blocking work
    // to `tokio::task::spawn_blocking`, which runs it on a dedicated thread pool.
    let flush_conn = Arc::clone(conn);
    let flush_interval = config.flush_interval_secs;
    let events_dir = config.events_dir();
    let flush_failures = Arc::clone(&state.flush_failures_total);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(flush_interval));
        loop {
            interval.tick().await;
            let conn = Arc::clone(&flush_conn);
            let dir = events_dir.clone();
            let result = tokio::task::spawn_blocking(move || {
                let conn_guard = conn.lock();
                let storage = ParquetStorage::new(&dir);
                storage.flush_events(&conn_guard)
            })
            .await;
            match result {
                Ok(Ok(count)) if count > 0 => {
                    tracing::info!(count, "Periodic flush completed");
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    flush_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::error!(error = %e, "Periodic flush failed");
                }
                Err(e) => {
                    flush_failures.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::error!(error = %e, "Periodic flush task panicked");
                }
            }
        }
    });

    // Data retention cleanup task (runs daily).
    //
    // `cleanup_old_partitions` calls `std::fs::read_dir` and `std::fs::remove_dir_all`
    // (blocking syscalls).  Wrapping with `spawn_blocking` matches the flush-task
    // pattern (L19) and prevents starving the async worker pool under load.
    if config.retention_days > 0 {
        let retention_events_dir = config.events_dir();
        let retention_days = config.retention_days;
        // ParquetStorage is cheap to clone (just a PathBuf), but constructing it
        // once outside the loop avoids a re-allocation on every daily iteration.
        let retention_storage = ParquetStorage::new(&retention_events_dir);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
            loop {
                interval.tick().await;
                let storage = retention_storage.clone();
                let result = tokio::task::spawn_blocking(move || {
                    storage.cleanup_old_partitions(retention_days)
                })
                .await;
                match result {
                    Ok(Ok(0)) => {}
                    Ok(Ok(removed)) => {
                        tracing::info!(removed, retention_days, "Data retention cleanup completed");
                    }
                    Ok(Err(e)) => {
                        tracing::error!(error = %e, "Data retention cleanup failed");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Data retention cleanup task panicked");
                    }
                }
            }
        });
    }

    // Session, cache, rate limiter, login tracker, and API key cleanup (runs every 15 minutes)
    let session_store = state.sessions.clone();
    let cache = state.query_cache.clone();
    let rl = state.rate_limiter.clone();
    let login_tracker = state.login_attempt_tracker.clone();
    let api_keys = state.api_keys.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
        loop {
            interval.tick().await;
            session_store.cleanup_expired();
            cache.cleanup_expired();
            rl.cleanup();
            login_tracker.cleanup();
            api_keys.cleanup_revoked();
        }
    });
}

async fn shutdown_signal(state: Arc<AppState>, timeout_secs: u64) {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => { tracing::info!("Received SIGINT"); },
        () = terminate => { tracing::info!("Received SIGTERM"); },
    }

    tracing::info!(
        timeout_secs,
        "Shutting down gracefully, flushing buffered events..."
    );

    // Flush remaining buffered events before shutdown, bounded by the configured timeout.
    let flush_fut = tokio::task::spawn_blocking({
        let state = Arc::clone(&state);
        move || state.buffer.flush()
    });

    let timeout = std::time::Duration::from_secs(timeout_secs.max(1));
    match tokio::time::timeout(timeout, flush_fut).await {
        Ok(Ok(Ok(count))) if count > 0 => {
            tracing::info!(count, "Flushed remaining events during shutdown");
        }
        Ok(Ok(Ok(_))) => {}
        Ok(Ok(Err(e))) => {
            tracing::error!(error = %e, "Failed to flush events during shutdown");
        }
        Ok(Err(e)) => {
            tracing::error!(error = %e, "Flush task panicked during shutdown");
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs,
                "Graceful shutdown flush timed out; some buffered events may be lost"
            );
        }
    }

    tracing::info!("Shutdown complete");
}
