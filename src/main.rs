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

    tracing::info!(
        host = %config.host,
        port = config.port,
        data_dir = %config.data_dir.display(),
        "Starting Mallard Metrics"
    );

    // Ensure data directory exists
    std::fs::create_dir_all(config.events_dir()).expect("Failed to create data directory");

    // Initialize DuckDB
    let conn = Connection::open_in_memory().expect("Failed to open DuckDB");
    storage::migrations::run_migrations(&conn).expect("Failed to run migrations");

    // Try to load the behavioral extension (non-fatal if unavailable)
    match storage::schema::load_behavioral_extension(&conn) {
        Ok(()) => tracing::info!("Behavioral extension loaded"),
        Err(e) => tracing::warn!(
            error = %e,
            "Behavioral extension not available; behavioral analytics features will be disabled"
        ),
    }

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

    let state = build_app_state(&config, buffer, geoip);

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

fn build_app_state(config: &Config, buffer: EventBuffer, geoip: GeoIpReader) -> Arc<AppState> {
    let sessions = SessionStore::new(config.session_ttl_secs);
    let api_keys = ApiKeyStore::new();
    let query_cache = crate::query::cache::QueryCache::new(config.cache_ttl_secs);
    let rate_limiter = crate::ingest::ratelimit::RateLimiter::new(config.rate_limit_per_site);

    let admin_password_hash = std::env::var("MALLARD_ADMIN_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty())
        .map(|p| {
            let hash = crate::api::auth::hash_password(&p).expect("Failed to hash admin password");
            tracing::info!("Admin password configured from MALLARD_ADMIN_PASSWORD");
            hash
        });

    Arc::new(AppState {
        buffer,
        secret: std::env::var("MALLARD_SECRET").unwrap_or_else(|_| {
            let secret = uuid::Uuid::new_v4().to_string();
            tracing::warn!("No MALLARD_SECRET set, using random secret: {secret}. Set MALLARD_SECRET for deterministic visitor IDs across restarts.");
            secret
        }),
        allowed_sites: config.site_ids.clone(),
        geoip,
        filter_bots: config.filter_bots,
        sessions,
        api_keys,
        admin_password_hash: Mutex::new(admin_password_hash),
        dashboard_origin: config.dashboard_origin.clone(),
        query_cache,
        rate_limiter,
    })
}

fn spawn_background_tasks(config: &Config, conn: &Arc<Mutex<Connection>>, state: &Arc<AppState>) {
    // Periodic flush task
    let flush_conn = Arc::clone(conn);
    let flush_interval = config.flush_interval_secs;
    let events_dir = config.events_dir();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(flush_interval));
        loop {
            interval.tick().await;
            let conn_guard = flush_conn.lock();
            let storage = ParquetStorage::new(&events_dir);
            match storage.flush_events(&conn_guard) {
                Ok(count) if count > 0 => {
                    tracing::info!(count, "Periodic flush completed");
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(error = %e, "Periodic flush failed");
                }
            }
        }
    });

    // Data retention cleanup task (runs daily)
    if config.retention_days > 0 {
        let retention_events_dir = config.events_dir();
        let retention_days = config.retention_days;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(24 * 60 * 60));
            loop {
                interval.tick().await;
                let storage = ParquetStorage::new(&retention_events_dir);
                match storage.cleanup_old_partitions(retention_days) {
                    Ok(0) => {}
                    Ok(removed) => {
                        tracing::info!(removed, retention_days, "Data retention cleanup completed");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Data retention cleanup failed");
                    }
                }
            }
        });
    }

    // Session, cache, and rate limiter cleanup task (runs every 15 minutes)
    let session_store = state.sessions.clone();
    let cache = state.query_cache.clone();
    let rl = state.rate_limiter.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15 * 60));
        loop {
            interval.tick().await;
            session_store.cleanup_expired();
            cache.cleanup_expired();
            rl.cleanup();
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
