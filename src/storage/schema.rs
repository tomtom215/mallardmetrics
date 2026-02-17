use duckdb::Connection;

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

/// Initialize the database schema.
pub fn init_schema(conn: &Connection) -> Result<(), duckdb::Error> {
    conn.execute_batch(CREATE_EVENTS_TABLE)?;
    Ok(())
}

/// Install and load the behavioral extension.
pub fn load_behavioral_extension(conn: &Connection) -> Result<(), duckdb::Error> {
    conn.execute_batch("INSTALL behavioral FROM community; LOAD behavioral;")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_schema() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Verify table exists by querying it
        let mut stmt = conn.prepare("SELECT COUNT(*) FROM events").unwrap();
        let count: i64 = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_init_schema_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap(); // Should not error
    }

    #[test]
    fn test_schema_columns() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();

        // Insert a row with all columns to verify schema
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
                "1920x1080",
                "US",
                "California",
                "San Francisco",
                r#"{"plan":"pro"}"#,
                99.99f64,
                "USD"
            ],
        )
        .unwrap();

        let mut stmt = conn.prepare("SELECT COUNT(*) FROM events").unwrap();
        let count: i64 = stmt.query_row([], |row| row.get(0)).unwrap();
        assert_eq!(count, 1);
    }
}
