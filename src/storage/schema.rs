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

/// Install and load the `behavioral` community extension.
///
/// # Errors
///
/// Returns an error if the extension cannot be downloaded or loaded — for
/// example when the host has no network access, or no build exists for the
/// running DuckDB version.
pub fn load_behavioral_extension(conn: &Connection) -> Result<(), duckdb::Error> {
    conn.execute_batch("INSTALL behavioral FROM community; LOAD behavioral;")
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

/// Create or refresh the `events_all` view unioning the hot table with Parquet.
///
/// ## Two-tier design
/// - **Hot tier** (`events` table): events received but not yet flushed.
/// - **Cold tier** (Parquet glob): everything flushed, in this run and previous ones.
///
/// ## Lifecycle
/// - Called at startup so historical data is queryable immediately.
/// - Called again after each flush that wrote files, so new Parquet appears.
/// - Falls back to a passthrough over `events` when no Parquet files exist yet.
///
/// `hive_partitioning=false` is required: the Parquet files already contain
/// `site_id` and `timestamp` columns, and Hive-style inference would add
/// duplicate `site_id`/`date` columns from the directory names, breaking the union.
///
/// # Errors
///
/// Returns an error if even the passthrough view cannot be created.
pub fn setup_query_view(conn: &Connection, parquet_dir: &Path) -> Result<(), duckdb::Error> {
    let columns = EVENT_COLUMNS.join(", ");
    let glob = format!(
        "{}/site_id=*/date=*/*.parquet",
        parquet_dir.to_string_lossy()
    );
    let escaped_glob = glob.replace('\'', "''");

    let union_sql = format!(
        "CREATE OR REPLACE VIEW events_all AS \
         SELECT {columns} FROM events \
         UNION ALL \
         SELECT {columns} FROM read_parquet('{escaped_glob}', union_by_name=true, hive_partitioning=false)"
    );

    if conn.execute_batch(&union_sql).is_ok() {
        return Ok(());
    }

    // No Parquet files yet (an unmatched glob errors on some builds and returns
    // zero rows on others) — fall back so queries still compile.
    conn.execute_batch(&format!(
        "CREATE OR REPLACE VIEW events_all AS SELECT {columns} FROM events"
    ))
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
    fn test_behavioral_version_is_none_without_extension() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        assert!(behavioral_version(&conn).is_none());
    }
}
