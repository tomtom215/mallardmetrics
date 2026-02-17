mod api;
mod config;
mod dashboard;
mod ingest;
mod query;
mod server;
mod storage;

use crate::config::Config;
use crate::ingest::buffer::EventBuffer;
use crate::ingest::handler::AppState;
use crate::storage::parquet::ParquetStorage;
use duckdb::Connection;
use parking_lot::Mutex;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mallard_metrics=info,tower_http=info".into()),
        )
        .init();

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

    let conn = Arc::new(Mutex::new(conn));
    let storage = ParquetStorage::new(&config.events_dir());
    let buffer = EventBuffer::new(config.flush_event_count, Arc::clone(&conn), storage);

    // Set up periodic flush
    let flush_conn = Arc::clone(&conn);
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

    let state = Arc::new(AppState {
        buffer,
        secret: std::env::var("MALLARD_SECRET").unwrap_or_else(|_| {
            let secret = uuid::Uuid::new_v4().to_string();
            tracing::warn!("No MALLARD_SECRET set, using random secret: {secret}. Set MALLARD_SECRET for deterministic visitor IDs across restarts.");
            secret
        }),
    });

    let app = server::build_router(state);
    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("Failed to bind to {addr}: {e}"));

    tracing::info!(addr = %addr, "Listening");
    axum::serve(listener, app).await.expect("Server error");
}
