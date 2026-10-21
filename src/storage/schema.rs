use duckdb::Connection;
use std::path::Path;

/// SQL statement to create the events table.
pub const CREATE_EVENTS_TABLE: &str = r"
CREATE TABLE IF NOT EXISTS events (
    site_id         VARCHAR NOT NULL,
    visitor_id      VARCHAR NOT NULL,
    timestamp       TIMESTAMP NOT NULL,
    event_name      VARCHAR NOT NULL,
    pathname        VARCHAR NOT NULL,
    hostname        VARCHAR,
    referrer        VARCHAR,
    referrer_source VARCHAR,
    utm_source      VARCHAR,
    utm_medium      VARCHAR,
    utm_campaign    VARCHAR,
    utm_content     VARCHAR,
    utm_term        VARCHAR,
    browser         VARCHAR,
    browser_version VARCHAR,
    os              VARCHAR,
    os_version      VARCHAR,
    device_type     VARCHAR,
    screen_size     VARCHAR,
    country_code    VARCHAR(2),
    region          VARCHAR,
    city            VARCHAR,
    props           VARCHAR,
    revenue_amount  DECIMAL(12,2),
    revenue_currency VARCHAR(3)
)
";

/// Column list of the `events` table, in declaration order.
///
/// `events_all` unions the hot table with Parquet files positionally, so both
/// sides must project the same columns in the same order. Naming them
/// explicitly (rather than `SELECT *`) means a future column added to the table
/// cannot silently misalign against Parquet files written by an older build.
pub const EVENT_COLUMNS: &[&str] = &[
    "site_id",
    "visitor_id",
    "timestamp",
    "event_name",
    "pathname",
    "hostname",
    "referrer",
    "referrer_source",
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_content",
    "utm_term",
    "browser",
    "browser_version",
    "os",
    "os_version",
    "device_type",
    "screen_size",
    "country_code",
    "region",
    "city",
    "props",
    "revenue_amount",
    "revenue_currency",
];

/// Initialize the database schema.
///
/// # Errors
///
/// Returns an error if the table cannot be created.
pub fn init_schema(conn: &Connection) -> Result<(), duckdb::Error> {
    conn.execute_batch(CREATE_EVENTS_TABLE)
}

/// Oldest `behavioral` release whose results Mallard's reports are written
/// against. 0.10.0 changed `window_funnel` to follow ClickHouse, and the funnel
/// SQL depends on the modes that release defines.
pub const MIN_BEHAVIORAL_VERSION: (u32, u32, u32) = (0, 10, 0);

/// Why the `behavioral` extension could not be put to use.
#[derive(Debug)]
pub enum BehavioralLoadError {
    /// `INSTALL` or `LOAD` failed: no network, no build for this platform, or
    /// an unwritable extension directory.
    Duckdb(duckdb::Error),
    /// The extension loaded, but it is older than [`MIN_BEHAVIORAL_VERSION`].
    ///
    /// `INSTALL` does nothing when a file is already present, so a data volume
    /// that cached an older build keeps serving it. The funnel results of an
    /// older build differ, so it is refused rather than used.
    TooOld(Option<String>),
}

impl std::fmt::Display for BehavioralLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duckdb(e) => write!(f, "{e}"),
            Self::TooOld(found) => {
                let (a, b, c) = MIN_BEHAVIORAL_VERSION;
                write!(
                    f,
                    "behavioral extension {} is older than the required {a}.{b}.{c}; \
                     delete the cached behavioral.duckdb_extension from the extension \
                     directory so it is downloaded again",
                    found.as_deref().unwrap_or("(version unknown)")
                )
            }
        }
    }
}

impl std::error::Error for BehavioralLoadError {}

impl From<duckdb::Error> for BehavioralLoadError {
    fn from(e: duckdb::Error) -> Self {
        Self::Duckdb(e)
    }
}

/// Parse `0.10.0` or `v0.10.0` (the extension has reported both spellings).
fn parse_version(raw: &str) -> Option<(u32, u32, u32)> {
    let mut parts = raw.trim().trim_start_matches('v').splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor, patch))
}

/// Install and load the `behavioral` community extension, and check its version.
///
/// # Errors
///
/// Returns an error if the extension cannot be downloaded or loaded — for
/// example when the host has no network access, or no build exists for the
/// running platform — or if the loaded build is older than
/// [`MIN_BEHAVIORAL_VERSION`].
pub fn load_behavioral_extension(conn: &Connection) -> Result<(), BehavioralLoadError> {
    conn.execute_batch("INSTALL behavioral FROM community; LOAD behavioral;")?;
    let version = behavioral_version(conn);
    match version.as_deref().and_then(parse_version) {
        Some(v) if v >= MIN_BEHAVIORAL_VERSION => Ok(()),
        _ => Err(BehavioralLoadError::TooOld(version)),
    }
}

/// Version string reported by the loaded `behavioral` extension.
///
/// Returns `None` when the extension is not loaded. Surfaced through
/// `/health/detailed` so an operator can tell which build is answering
/// funnel/retention/session queries.
pub fn behavioral_version(conn: &Connection) -> Option<String> {
    conn.query_row("SELECT behavioral_version()", [], |row| row.get(0))
        .ok()
}

/// Directory keys as explicit types; see [`setup_query_view`].
const HIVE_OPTIONS: &str = "hive_partitioning=true, hive_types={'site_id': VARCHAR, 'date': DATE}";

/// Text DuckDB puts in the error for a glob that matches no file. Checked
/// against DuckDB 1.5.6 by `test_unmatched_glob_error_has_the_expected_text`;
/// if an upgrade changes it that test fails rather than the fallback below
/// silently starting to swallow other errors.
const NO_FILES_MARKER: &str = "No files found that match the pattern";

/// [`NO_FILES_MARKER`] for the test that pins it.
#[cfg(test)]
pub(crate) const NO_FILES_MARKER_FOR_TESTS: &str = NO_FILES_MARKER;

/// Create or refresh the `events_all` view unioning the hot table with Parquet.
///
/// ## Two-tier design
/// - **Hot tier** (`events` table): events received but not yet flushed.
/// - **Cold tier** (Parquet glob): everything flushed, in this run and previous ones.
///
/// ## Lifecycle
/// - Called at startup so historical data is queryable immediately.
/// - Called again after a flush into a database whose view was built while the
///   cold tier was empty (see [`ensure_query_view`]).
/// - Called after retention or erasure removes partitions: a view over a glob
///   that now matches nothing makes every query fail (F-05).
/// - Falls back to a passthrough over `events` only when the glob matches no
///   file at all, which is the normal state of a fresh install.
///
/// ## Hive partitioning
/// The files carry `site_id` as a column and also sit under `site_id=*/date=*`.
/// Inferred hive typing is not trusted (it guesses types and would add `date`
/// to the column set), so the view reads the directory keys with explicit types
/// and projects the explicit [`EVENT_COLUMNS`] list, which drops `date`. When a
/// name is in both the file and the path DuckDB keeps the file's column
/// (checked on DuckDB 1.5.6 and pinned by
/// `test_hive_view_returns_exactly_what_the_plain_glob_does`), so results are
/// unchanged; the path keys let the planner skip files for a `site_id`
/// predicate. Measured at 100 sites x 90 days with the footer cache: 1.36-1.43 s
/// before, 0.59-0.63 s after (`cargo bench -- query_scale`).
///
/// # Errors
///
/// Returns an error if the union view cannot be created for any reason other
/// than "no files yet" (a corrupt or foreign file in the tree, a schema
/// conflict, a permission problem, a data directory containing glob
/// characters). In that case a hot-only view is still installed so ingestion
/// and the hot tier keep working, but the error is returned and
/// [`cold_tier_status`] reports `Degraded`: analytics would otherwise silently
/// lose the entire cold tier (F-04). Also returns an error if even the
/// passthrough view cannot be created.
pub fn setup_query_view(conn: &Connection, parquet_dir: &Path) -> Result<(), duckdb::Error> {
    let columns = EVENT_COLUMNS.join(", ");
    let hot_only = format!("CREATE OR REPLACE VIEW events_all AS SELECT {columns} FROM events");

    if let Err(reason) = crate::storage::parquet::validate_data_dir(parquet_dir) {
        conn.execute_batch(&hot_only)?;
        return Err(duckdb::Error::InvalidParameterName(reason));
    }

    let glob = format!(
        "{}/site_id=*/date=*/*.parquet",
        parquet_dir.to_string_lossy()
    );
    let escaped_glob = glob.replace('\'', "''");

    let union_sql = format!(
        "CREATE OR REPLACE VIEW events_all AS \
         SELECT {columns} FROM events \
         UNION ALL \
         SELECT {columns} FROM read_parquet('{escaped_glob}', union_by_name=true, {HIVE_OPTIONS})"
    );

    match conn.execute_batch(&union_sql) {
        Ok(()) => {
            enable_footer_cache(conn);
            Ok(())
        }
        Err(e) if e.to_string().contains(NO_FILES_MARKER) => conn.execute_batch(&hot_only),
        Err(e) => {
            tracing::error!(
                error = %e,
                "Could not build the events_all view over the Parquet files; analytics \
                 will see only unflushed events until this is fixed"
            );
            // Keep the hot tier queryable. The failure is returned either way.
            if let Err(fallback) = conn.execute_batch(&hot_only) {
                tracing::error!(error = %fallback, "Could not install the hot-only fallback view");
            }
            Err(e)
        }
    }
}

/// A parenthesised, aliasable replacement for `events_all` that reads only
/// `files` from the cold tier, plus the whole hot table (F-06).
///
/// `events_all` expands a glob over every site and day on every query, and
/// `union_by_name` opens every footer to build the schema; at 100 sites x 90
/// days that is 9,000 files per request however narrow the filter. A request
/// that knows its site and dates can pass
/// [`ParquetStorage::files_for_range`](crate::storage::parquet::ParquetStorage::files_for_range)
/// and read tens of files. Results are identical to `events_all` filtered to
/// the same site and range, because partitions are keyed by site and event
/// date. Callers that need history beyond the range (sessions, retention
/// cohorts) must widen the range, or keep using `events_all`.
///
/// The paths are file names built by this crate from validated components and
/// are quote-escaped; `COPY`/`read_parquet` arguments cannot be bound (L1).
/// With no files it is the hot table alone, which is correct for a range with
/// nothing flushed.
pub fn scoped_events_sql(files: &[std::path::PathBuf]) -> String {
    let columns = EVENT_COLUMNS.join(", ");
    if files.is_empty() {
        return format!("(SELECT {columns} FROM events)");
    }
    let list = files
        .iter()
        .map(|f| format!("'{}'", f.to_string_lossy().replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "(SELECT {columns} FROM events UNION ALL \
         SELECT {columns} FROM read_parquet([{list}], union_by_name=true, hive_partitioning=false))"
    )
}

/// Keep Parquet footers in memory between queries (F-06).
///
/// Every query re-reads the footer of every file the view matches, because
/// `union_by_name` needs each file's schema. With the cache on, a repeat query
/// skips those reads; DuckDB validates an entry against the file's size and
/// modification time, so a replaced file is re-read (pinned by
/// `test_footer_cache_notices_a_replaced_file`). Measured at 100 sites x 90
/// days: see the `query_scale` benchmark. `GLOBAL` so reader connections
/// cloned from this one share it. Failure to set it only costs speed.
fn enable_footer_cache(conn: &Connection) {
    if let Err(e) = conn.execute_batch("SET GLOBAL parquet_metadata_cache = true") {
        tracing::warn!(error = %e, "Could not enable the Parquet footer cache");
    }
}

/// True when `events_all` currently reads the Parquet tier.
fn view_has_cold_tier(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM duckdb_views() \
         WHERE view_name = 'events_all' AND sql ILIKE '%read_parquet%'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .is_ok_and(|n| n > 0)
}

/// Rebuild `events_all` only if it does not read the Parquet tier yet.
///
/// The union view re-expands its glob on every query, so once it includes the
/// cold tier a flush never needs to touch it, and rebuilding is not free:
/// `CREATE VIEW` binds the `read_parquet` call, which with `union_by_name`
/// opens every file's footer. Flush used to do that on each run, under the
/// exclusive tier lock.
///
/// # Errors
///
/// As for [`setup_query_view`].
pub fn ensure_query_view(conn: &Connection, parquet_dir: &Path) -> Result<(), duckdb::Error> {
    if view_has_cold_tier(conn) {
        Ok(())
    } else {
        setup_query_view(conn, parquet_dir)
    }
}

/// Whether the cold tier is reachable from analytics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColdTierStatus {
    /// `events_all` reads the Parquet files and there are files to read.
    Healthy,
    /// No Parquet files exist yet; hot-only is correct.
    Empty,
    /// Files exist on disk but `events_all` does not read them (the union view
    /// could not be built), or it reads a glob that no longer matches anything.
    /// Analytics results are incomplete or failing. Health checks should
    /// report this.
    Degraded,
}

/// Inspect the view and the data directory. Cheap enough for a health check:
/// it stops at the first Parquet file it finds.
pub fn cold_tier_status(conn: &Connection, parquet_dir: &Path) -> ColdTierStatus {
    match (view_has_cold_tier(conn), has_parquet_files(parquet_dir)) {
        (true, true) => ColdTierStatus::Healthy,
        (false, false) => ColdTierStatus::Empty,
        _ => ColdTierStatus::Degraded,
    }
}

/// True if any `site_id=*/date=*/*.parquet` file exists under `dir`.
fn has_parquet_files(dir: &Path) -> bool {
    let subdirs = |path: &Path| -> Vec<std::path::PathBuf> {
        std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect()
    };
    subdirs(dir).into_iter().any(|site| {
        subdirs(&site).into_iter().any(|date| {
            std::fs::read_dir(date)
                .into_iter()
                .flatten()
                .flatten()
                .any(|f| f.path().extension().is_some_and(|ext| ext == "parquet"))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_setup_query_view_no_parquet() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let dir = tempfile::tempdir().unwrap();
        setup_query_view(&conn, dir.path()).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events_all", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_init_schema() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_init_schema_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
    }

    #[test]
    fn test_event_columns_match_the_table_exactly() {
        // Guards the positional UNION ALL in events_all: if a column is added to
        // CREATE_EVENTS_TABLE without updating EVENT_COLUMNS, the hot and cold
        // tiers would silently misalign.
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_name = 'events' ORDER BY ordinal_position",
            )
            .unwrap();
        let actual: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .filter_map(Result::ok)
            .collect();
        assert_eq!(actual, EVENT_COLUMNS, "EVENT_COLUMNS is out of date");
    }

    #[test]
    fn test_schema_columns_accept_a_full_row() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname,
             hostname, referrer, referrer_source, utm_source, utm_medium,
             utm_campaign, utm_content, utm_term, browser, browser_version,
             os, os_version, device_type, screen_size, country_code,
             region, city, props, revenue_amount, revenue_currency)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            duckdb::params![
                "example.com",
                "abc123",
                "2024-01-15 10:30:00",
                "pageview",
                "/",
                "example.com",
                "https://google.com",
                "Google",
                "google",
                "organic",
                "winter",
                "banner",
                "analytics",
                "Chrome",
                "120.0",
                "Windows",
                "11",
                "desktop",
                "1920",
                "US",
                "California",
                "San Francisco",
                r#"{"plan":"pro"}"#,
                99.99f64,
                "USD"
            ],
        )
        .unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_parse_version_accepts_both_spellings() {
        assert_eq!(parse_version("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("v0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_version("0.9.1"), Some((0, 9, 1)));
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("garbage"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn test_versions_compare_numerically_not_lexically() {
        assert!(parse_version("0.9.1") < Some(MIN_BEHAVIORAL_VERSION));
        assert!(parse_version("0.10.0") >= Some(MIN_BEHAVIORAL_VERSION));
        assert!(parse_version("0.100.0") > Some(MIN_BEHAVIORAL_VERSION));
    }

    #[test]
    fn test_behavioral_version_is_none_without_extension() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        assert!(behavioral_version(&conn).is_none());
    }
}
