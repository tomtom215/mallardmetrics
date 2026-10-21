//! Mallard Metrics server entry point.
//!
//! The binary drives the library crate rather than re-declaring the module
//! tree, so everything is compiled once instead of twice.

use mallard_metrics::api::auth::{
    AdminPasswordStore, ApiKeyStore, LoginAttemptTracker, SessionStore, write_private_file,
};
use mallard_metrics::config::Config;
use mallard_metrics::ingest::buffer::EventBuffer;
use mallard_metrics::ingest::geoip::GeoIpReader;
use mallard_metrics::ingest::handler::AppState;
use mallard_metrics::ingest::ratelimit::RateLimiter;
use mallard_metrics::query::cache::QueryCache;
use mallard_metrics::storage::parquet::ParquetStorage;
use mallard_metrics::storage::{ReaderPool, TierLock};
use mallard_metrics::{query, server, storage};

use duckdb::Connection;
use parking_lot::Mutex;
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
        Some("--install-extension") => std::process::exit(run_install_extension()),
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
             mallard-metrics --healthcheck\n    \
             mallard-metrics --install-extension\n\n\
         OPTIONS:\n    \
             CONFIG_FILE           Path to a TOML configuration file (optional)\n    \
             --healthcheck         Probe a running instance and exit 0 when healthy\n    \
             --install-extension   Download the DuckDB `behavioral` extension into\n                          \
                                   the data directory and exit. For deployments\n                          \
                                   whose runtime network has no outbound route:\n                          \
                                   run this once with one, then start normally.\n    \
             --version, -V         Print the version and exit\n    \
             --help, -h            Print this message\n\n\
         Configuration may also be supplied through MALLARD_* environment\n\
         variables; see mallard-metrics.toml.example for the full list.",
        version = env!("CARGO_PKG_VERSION")
    );
}

/// Download and load the `behavioral` extension, then exit.
///
/// The extension is fetched over HTTPS the first time a database needs it, and
/// stored under the configured extension directory — which is on the data
/// volume, so it persists. That matters for any deployment whose runtime
/// network has no outbound route: the bundled production compose file puts
/// Mallard on an `internal: true` network specifically so a compromised
/// container cannot reach the internet, and the extension can therefore never
/// install itself there. Funnels, retention, sessions, sequences and flow would
/// answer 503 forever, on a deployment that is otherwise entirely healthy.
///
/// Running this once with a route, against the same data directory, seeds the
/// file; the isolated deployment then loads it from disk and needs no network.
/// Doing it through the real installer rather than by hand also writes the
/// `.info` metadata DuckDB uses to decide the file is loadable.
///
/// Returns a process exit code: 0 when the extension is available afterwards.
fn run_install_extension() -> i32 {
    let loaded = match Config::load(None) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("Configuration error: {e}");
            return EXIT_FAILURE;
        }
    };
    let config = loaded.config;

    if let Err(e) = std::fs::create_dir_all(&config.data_dir) {
        eprintln!(
            "Could not create the data directory {}: {e}",
            config.data_dir.display()
        );
        return EXIT_FAILURE;
    }

    // Opened in memory: seeding the extension must not create or migrate a
    // database file, which would leave a half-initialised one behind if the
    // real deployment is configured differently. The extension directory is a
    // database-level setting, so an in-memory database installs to the same
    // place a persistent one would.
    let conn = match Connection::open_in_memory() {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("Could not open DuckDB: {e}");
            return EXIT_FAILURE;
        }
    };
    // No tracing subscriber exists on this path, so the `info!` inside is a
    // no-op and the result is reported to stdout below instead.
    if let Err(e) = apply_duckdb_settings(&conn, &config) {
        eprintln!("{e}");
        return EXIT_FAILURE;
    }

    match storage::schema::load_behavioral_extension(&conn) {
        Ok(()) => {
            let version = storage::schema::behavioral_version(&conn);
            println!(
                "Installed the behavioral extension{} into {}",
                version.map_or_else(String::new, |v| format!(" (version {v})")),
                config.extension_dir().display()
            );
            println!("Start the server normally; it will load the extension from disk.");
            0
        }
        Err(e) => {
            eprintln!("Could not install the behavioral extension: {e}");
            eprintln!(
                "This step needs outbound HTTP (plain port 80, not HTTPS) to \
                 community-extensions.duckdb.org. Run it from a host or network that has one, against the \
                 same data directory."
            );
            EXIT_FAILURE
        }
    }
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

    let shutdown_timeout = std::time::Duration::from_secs(config.shutdown_timeout_secs.max(1));

    // The final flush has to come after the server has drained, not before.
    // `with_graceful_shutdown` stops accepting connections when its future
    // completes, but requests already in flight keep running — and an event
    // they accept lands in a buffer that a flush started at the signal has
    // already emptied. So the future below only waits for the signal; the flush
    // runs once `serve` returns.
    //
    // `server::serve` is `axum::serve` plus a header-read timeout (SI-14): the
    // peer address still reaches handlers as ConnectInfo, which the client-IP
    // fallback uses when no proxy headers are trusted, and shutdown still stops
    // accepting and then drains.
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();
    let serving = server::serve(listener, app, server::HEADER_READ_TIMEOUT, async move {
        wait_for_shutdown_signal().await;
        // The receiver only goes away if `serve` has already returned.
        let _ = signalled_tx.send(());
    });
    tokio::pin!(serving);

    // A connection that never finishes must not keep the process alive past the
    // drain budget: orchestrators follow SIGTERM with SIGKILL, and the flush
    // below still has to run.
    let outcome = tokio::select! {
        result = &mut serving => Some(result),
        () = async {
            if signalled_rx.await.is_ok() {
                tokio::time::sleep(shutdown_timeout).await;
            } else {
                std::future::pending::<()>().await;
            }
        } => {
            tracing::warn!(
                timeout_secs = shutdown_timeout.as_secs(),
                "Connections did not drain in time; shutting down with some still open"
            );
            None
        }
    };

    flush_on_shutdown(&state, shutdown_timeout).await;

    if let Some(Err(e)) = outcome {
        tracing::error!(error = %e, "Server error");
        std::process::exit(EXIT_FAILURE);
    }
    tracing::info!("Shutdown complete");
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
    /// Loaded on the writer and on every reader.
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
    apply_duckdb_settings(&conn, config)?;
    storage::migrations::run_migrations(&conn).map_err(|e| format!("migrations failed: {e}"))?;

    let writer_loaded = match storage::schema::load_behavioral_extension(&conn) {
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
                 sequences and flow analysis will report 503 until it loads \
                 (it is retried every {} s)",
                query::extension::RETRY_INTERVAL.as_secs()
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

    // A flush interrupted after its rows were committed to Parquet staging but
    // before the file was published is finished here, before the view exists.
    match storage.recover_pending_flushes(&conn) {
        Ok(report) => tracing::info!(?report, "Checked for interrupted flushes"),
        Err(e) => tracing::warn!(error = %e, "Could not recover interrupted flushes"),
    }

    if let Err(e) = storage::schema::setup_query_view(&conn, &events_dir) {
        tracing::warn!(error = %e, "Could not create the events_all view; queries will be limited");
    }

    let writer = Arc::new(Mutex::new(conn));
    let readers = ReaderPool::new(&writer, config.read_connections);
    // The view the writer just created is already in the shared catalog, and
    // `LOAD` on the writer arms readers cloned from it too (pinned by
    // `query::extension::tests::loading_on_the_writer_arms_cloned_readers`).
    // A query is handed whichever reader is next, so "loaded" is still asked of
    // every connection rather than of the writer alone, and a failure is
    // reported: it used to be dropped (`let _ =`), and the first one ended the
    // loop.
    let behavioral_loaded = if writer_loaded && readers.len() > 1 {
        let report = query::extension::load_everywhere(&writer, &readers);
        if !report.complete() {
            tracing::warn!(
                failed = report.attempted - report.loaded,
                of = readers.len(),
                "Behavioral extension did not load on every read connection; \
                 reporting it as unavailable and retrying"
            );
        }
        report.complete()
    } else {
        writer_loaded
    };

    Ok(Database {
        writer,
        readers,
        behavioral_loaded,
        behavioral_version,
    })
}

/// Apply the deployment's DuckDB settings to the freshly opened database.
///
/// These are database-wide rather than per-connection, so the writer is the
/// only place they need to be set; the reader pool clones from it.
///
/// `extension_directory` is the one that matters most. DuckDB defaults to
/// `$HOME/.duckdb/extensions`, and the shipped `FROM scratch` image sets no
/// `HOME` and is documented to run with a read-only root filesystem — so
/// `INSTALL behavioral FROM community` had nowhere to write and every
/// behavioral endpoint answered 503 on an otherwise healthy deployment.
fn apply_duckdb_settings(conn: &Connection, config: &Config) -> Result<(), String> {
    let extension_dir = config.extension_dir();
    std::fs::create_dir_all(&extension_dir).map_err(|e| {
        format!(
            "could not create the extension directory {}: {e}",
            extension_dir.display()
        )
    })?;

    // `home_directory` has to be set as well, and this is not belt-and-braces.
    // DuckDB resolves the home directory while installing an extension even
    // when `extension_directory` points somewhere else, and with no `HOME` in
    // the environment that resolution fails outright:
    //
    //     IO Error: Can't find the home directory at ''
    //
    // which is exactly what a `FROM scratch` image produces. Pointing both at
    // the data volume is what actually makes the install work there; setting
    // only the extension directory does not.
    let escaped_home = config.data_dir.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("SET home_directory = '{escaped_home}'"))
        .map_err(|e| format!("could not set home_directory: {e}"))?;

    let escaped = extension_dir.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("SET extension_directory = '{escaped}'"))
        .map_err(|e| format!("could not set extension_directory: {e}"))?;

    if let Some(limit) = &config.duckdb_memory_limit {
        // Validated by `Config::validate`, and re-escaped rather than trusted.
        let escaped = limit.replace('\'', "''");
        conn.execute_batch(&format!("SET memory_limit = '{escaped}'"))
            .map_err(|e| format!("could not set memory_limit to {limit:?}: {e}"))?;
    }
    if let Some(threads) = config.duckdb_threads {
        conn.execute_batch(&format!("SET threads = {threads}"))
            .map_err(|e| format!("could not set threads to {threads}: {e}"))?;
    }

    tracing::info!(
        home_directory = %config.data_dir.display(),
        extension_directory = %extension_dir.display(),
        memory_limit = config.duckdb_memory_limit.as_deref().unwrap_or("(DuckDB default)"),
        threads = config.duckdb_threads.map_or_else(|| "(one per core)".to_string(), |t| t.to_string()),
        "DuckDB configured"
    );
    Ok(())
}

/// Assemble the shared application state.
fn build_state(config: &Config) -> Result<Arc<AppState>, String> {
    let events_dir = config.events_dir();
    let storage = ParquetStorage::new(&events_dir, config.compact_after_files);
    let db = open_database(config, &storage)?;

    let tier_lock = TierLock::new();
    let buffer = EventBuffer::new(
        config.flush_event_count,
        config.max_buffered_events,
        Arc::clone(&db.writer),
        storage,
        tier_lock.clone(),
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
    let query_limits = config.query_limits();

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
        tier_lock,
        secret: load_or_create_secret(config),
        allowed_sites: config.site_ids.clone(),
        geoip: GeoIpReader::open(config.geoip_db_path.as_deref()),
        datacenter: open_datacenter_filter(config)?,
        filter_bots: config.filter_bots,
        sessions: SessionStore::with_capacity(config.session_ttl_secs, config.max_sessions),
        api_keys: ApiKeyStore::load_from_disk(config.data_dir.join("api_keys.json")),
        admin_password: AdminPasswordStore::load_from_disk(
            config.data_dir.join("admin.json"),
            load_admin_password(),
        ),
        dashboard_origin: config.dashboard_origin.clone(),
        query_cache: QueryCache::new(config.cache_ttl_secs, config.cache_max_entries),
        rate_limiter: RateLimiter::new(config.rate_limit_per_site, config.max_tracked_keys),
        ip_rate_limiter: RateLimiter::new(config.rate_limit_per_ip, config.max_tracked_keys),
        login_attempt_tracker: LoginAttemptTracker::with_capacity(
            config.max_login_attempts,
            config.login_lockout_secs,
            config.max_tracked_keys,
        ),
        http_metrics: Arc::default(),
        events_ingested_total: Arc::new(AtomicU64::new(0)),
        flush_failures_total: Arc::new(AtomicU64::new(0)),
        last_flush_success_unix: Arc::new(AtomicU64::new(0)),
        started_at_unix: unix_now(),
        rate_limit_rejections_total: Arc::new(AtomicU64::new(0)),
        login_failures_total: Arc::new(AtomicU64::new(0)),
        metrics_token,
        query_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent)),
        query_limits,
        secure_cookies: config.secure_cookies,
        behavioral: query::extension::ExtensionState::new(
            db.behavioral_loaded,
            db.behavioral_version,
        ),
        trust_proxy_headers: config.trust_proxy_headers,
        trusted_proxy_hops: config.trusted_proxy_hops,
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
        data_dir: config.data_dir.clone(),
        backup_dir: config.data_dir.join("backups"),
    }))
}

/// The datacenter filter: off unless `asn_db_path` is set, and a database that
/// cannot be used stops startup (a silent downgrade would look like filtering).
fn open_datacenter_filter(
    config: &Config,
) -> Result<mallard_metrics::ingest::asn::DatacenterFilter, String> {
    use mallard_metrics::ingest::asn::DatacenterFilter;
    let Some(path) = config.asn_db_path.as_deref() else {
        return Ok(DatacenterFilter::disabled());
    };
    let filter = DatacenterFilter::open(path, &config.datacenter_asns)?;
    tracing::info!(
        path = %path.display(),
        asns = filter.asn_count(),
        "Datacenter traffic filter enabled"
    );
    Ok(filter)
}

/// Hash the admin password supplied through the environment, if any.
fn load_admin_password() -> Option<String> {
    let password = std::env::var("MALLARD_ADMIN_PASSWORD")
        .ok()
        .filter(|p| !p.is_empty())?;

    // The same policy as the setup endpoint: an environment variable is not a
    // way around the minimum length.
    if let Err(msg) = mallard_metrics::api::auth::validate_password(&password) {
        eprintln!("MALLARD_ADMIN_PASSWORD rejected: {msg}");
        std::process::exit(EXIT_FAILURE);
    }

    match mallard_metrics::api::auth::hash_password(&password) {
        Ok(hash) => {
            tracing::info!("Admin password configured from MALLARD_ADMIN_PASSWORD");
            Some(hash)
        }
        Err(e) => {
            // Starting anyway would run an unauthenticated dashboard for an
            // operator who explicitly asked for a password, so refuse to start.
            eprintln!("Failed to hash MALLARD_ADMIN_PASSWORD: {e}");
            std::process::exit(EXIT_FAILURE);
        }
    }
}

/// Shortest accepted `MALLARD_SECRET`.
const MIN_SECRET_CHARS: usize = 16;

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
            // The visitor ID is an HMAC keyed with this value; a short, guessable
            // key lets anyone who sees an ID brute-force the IP behind it.
            if secret.chars().count() < MIN_SECRET_CHARS {
                eprintln!(
                    "MALLARD_SECRET is shorter than {MIN_SECRET_CHARS} characters. \
                     Use a random value, e.g. `openssl rand -hex 32`."
                );
                std::process::exit(EXIT_FAILURE);
            }
            return secret;
        }
    }

    let secret_path = config.data_dir.join(".secret");
    // Clear temp files an interrupted write left beside the secret.
    mallard_metrics::api::auth::remove_stale_temp_files(&secret_path);
    match std::fs::read_to_string(&secret_path) {
        Ok(existing) if !existing.trim().is_empty() => {
            mallard_metrics::api::auth::restrict_to_owner(&secret_path);
            tracing::info!(path = %secret_path.display(), "Loaded the persisted visitor-ID secret");
            return existing.trim().to_string();
        }
        // An empty file carries no secret; fall through and generate one.
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            // Generating a fresh secret here would replace the one on disk and
            // silently break every visitor ID derived from it.
            eprintln!(
                "Cannot read {}: {e}; refusing to replace the visitor-ID secret",
                secret_path.display()
            );
            std::process::exit(EXIT_FAILURE);
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
    if !state.behavioral.is_loaded() {
        // Started without the extension on every connection (no network at
        // boot, a reader that failed to load): keep trying instead of serving
        // 503 until the next restart.
        tokio::spawn(query::extension::retry_until_loaded(
            Arc::clone(state),
            query::extension::RETRY_INTERVAL,
        ));
    }
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
                Ok(Ok(count)) => {
                    state
                        .last_flush_success_unix
                        .store(unix_now(), Ordering::Relaxed);
                    if count > 0 {
                        tracing::info!(count, "Periodic flush completed");
                    }
                }
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

/// Seconds since the Unix epoch. Zero if the clock is before it, which a
/// monotonic-failure metric can only read as "never".
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Delete Parquet partitions past the retention horizon, daily.
fn spawn_retention_task(retention_days: u32, state: &Arc<AppState>) {
    let storage = state.buffer.storage().clone();
    let writer = state.buffer.conn().clone();
    let tier_lock = state.tier_lock.clone();
    let cache_state = Arc::clone(state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RETENTION_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let storage = storage.clone();
            let writer = writer.clone();
            let tier_lock = tier_lock.clone();
            let result = tokio::task::spawn_blocking(move || {
                // The read glob is expanded when a query starts and the files
                // are opened after that, so a partition removed in between
                // makes the query fail on a file that no longer exists. Taking
                // the tier lock keeps deletions out of that window.
                let _tier = tier_lock.write();
                // Refreshes the view too: removing the last partition would
                // otherwise leave `events_all` reading a glob that matches nothing.
                storage.cleanup_old_partitions_and_refresh(&writer.lock(), retention_days)
            })
            .await;
            match result {
                Ok(Ok(0)) => {}
                Ok(Ok(removed)) => {
                    // Reports cached before the cleanup still include what was removed.
                    cache_state.query_cache.clear();
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

/// Wait for SIGINT or SIGTERM.
async fn wait_for_shutdown_signal() {
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
}

/// Flush whatever is still buffered, within `timeout`.
///
/// Called after the server has drained, so no handler can add to the buffer
/// behind it.
async fn flush_on_shutdown(state: &Arc<AppState>, timeout: std::time::Duration) {
    tracing::info!(
        timeout_secs = timeout.as_secs(),
        buffered = state.buffer.len(),
        "Shutting down gracefully, flushing buffered events"
    );

    // `shutdown_flush` also closes the buffer, so nothing can be pushed behind it.
    let flush = tokio::task::spawn_blocking({
        let state = Arc::clone(state);
        move || state.buffer.shutdown_flush()
    });

    match tokio::time::timeout(timeout, flush).await {
        Ok(Ok(Ok(count))) if count > 0 => {
            tracing::info!(count, "Flushed remaining events during shutdown");
        }
        Ok(Ok(Ok(_))) => {}
        Ok(Ok(Err(e))) => tracing::error!(error = %e, "Failed to flush events during shutdown"),
        Ok(Err(e)) => tracing::error!(error = %e, "Flush task panicked during shutdown"),
        Err(_) => tracing::warn!(
            timeout_secs = timeout.as_secs(),
            "Graceful shutdown flush timed out; some buffered events may be lost"
        ),
    }
}
