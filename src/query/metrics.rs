use duckdb::Connection;

/// Core metric results for a given time range.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CoreMetrics {
    pub unique_visitors: u64,
    pub total_pageviews: u64,
    pub bounce_rate: f64,
    pub avg_visit_duration_secs: f64,
    pub pages_per_visit: f64,
}

/// Query core metrics for a site within a date range.
pub fn query_core_metrics(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
) -> Result<CoreMetrics, duckdb::Error> {
    let unique_visitors = query_unique_visitors(conn, site_id, start_date, end_date)?;
    let total_pageviews = query_total_pageviews(conn, site_id, start_date, end_date)?;
    // bounce_rate requires the behavioral extension (sessionize).
    // Gracefully return 0.0 if the extension is not loaded.
    let bounce_rate = query_bounce_rate(conn, site_id, start_date, end_date).unwrap_or(0.0);

    let pages_per_visit = if unique_visitors > 0 {
        #[allow(clippy::cast_precision_loss)]
        let pv = total_pageviews as f64 / unique_visitors as f64;
        pv
    } else {
        0.0
    };

    Ok(CoreMetrics {
        unique_visitors,
        total_pageviews,
        bounce_rate,
        avg_visit_duration_secs: 0.0, // Requires sessionize, computed in sessions module
        pages_per_visit,
    })
}

/// Count unique visitors in a date range.
pub fn query_unique_visitors(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
) -> Result<u64, duckdb::Error> {
    let mut stmt = conn.prepare(
        "SELECT COUNT(DISTINCT visitor_id) FROM events
         WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)",
    )?;
    let count: u64 = stmt.query_row(duckdb::params![site_id, start_date, end_date], |row| {
        row.get(0)
    })?;
    Ok(count)
}

/// Count total pageviews in a date range.
pub fn query_total_pageviews(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
) -> Result<u64, duckdb::Error> {
    let mut stmt = conn.prepare(
        "SELECT COUNT(*) FROM events
         WHERE site_id = ? AND event_name = 'pageview'
         AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)",
    )?;
    let count: u64 = stmt.query_row(duckdb::params![site_id, start_date, end_date], |row| {
        row.get(0)
    })?;
    Ok(count)
}

/// Calculate bounce rate using sessionize from the behavioral extension.
///
/// Returns a value between 0.0 and 1.0, or 0.0 if no sessions exist.
pub fn query_bounce_rate(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
) -> Result<f64, duckdb::Error> {
    let sql = r"
        WITH sessions AS (
            SELECT
                visitor_id,
                sessionize(timestamp, INTERVAL '30 minutes') OVER (
                    PARTITION BY visitor_id ORDER BY timestamp
                ) AS session_id,
                event_name
            FROM events
            WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)
        )
        SELECT
            COALESCE(
                COUNT(DISTINCT CASE WHEN page_count = 1 THEN session_key END)::FLOAT
                / NULLIF(COUNT(DISTINCT session_key), 0),
                0.0
            ) AS bounce_rate
        FROM (
            SELECT
                visitor_id || '-' || CAST(session_id AS VARCHAR) AS session_key,
                COUNT(*) FILTER (WHERE event_name = 'pageview') AS page_count
            FROM sessions
            GROUP BY visitor_id, session_id
        )
    ";

    let mut stmt = conn.prepare(sql)?;
    let bounce_rate: f64 = stmt
        .query_row(duckdb::params![site_id, start_date, end_date], |row| {
            row.get(0)
        })?;
    Ok(bounce_rate)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        conn
    }

    fn insert_pageview(conn: &Connection, visitor_id: &str, timestamp: &str, pathname: &str) {
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES ('test.com', ?, CAST(? AS TIMESTAMP), 'pageview', ?)",
            duckdb::params![visitor_id, timestamp, pathname],
        )
        .unwrap();
    }

    #[test]
    fn test_unique_visitors_empty() {
        let conn = setup_test_db();
        let count = query_unique_visitors(&conn, "test.com", "2024-01-01", "2024-02-01").unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_unique_visitors_counting() {
        let conn = setup_test_db();
        insert_pageview(&conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&conn, "v1", "2024-01-15 10:05:00", "/about");
        insert_pageview(&conn, "v2", "2024-01-15 11:00:00", "/");

        let count = query_unique_visitors(&conn, "test.com", "2024-01-01", "2024-02-01").unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_unique_visitors_date_range() {
        let conn = setup_test_db();
        insert_pageview(&conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&conn, "v2", "2024-02-15 10:00:00", "/");

        // Only January
        let count = query_unique_visitors(&conn, "test.com", "2024-01-01", "2024-02-01").unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_total_pageviews() {
        let conn = setup_test_db();
        insert_pageview(&conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&conn, "v1", "2024-01-15 10:05:00", "/about");
        insert_pageview(&conn, "v2", "2024-01-15 11:00:00", "/");

        let count = query_total_pageviews(&conn, "test.com", "2024-01-01", "2024-02-01").unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn test_total_pageviews_excludes_custom_events() {
        let conn = setup_test_db();
        insert_pageview(&conn, "v1", "2024-01-15 10:00:00", "/");
        conn.execute(
            "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
             VALUES ('test.com', 'v1', '2024-01-15 10:01:00', 'signup', '/')",
            [],
        )
        .unwrap();

        let count = query_total_pageviews(&conn, "test.com", "2024-01-01", "2024-02-01").unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_core_metrics_empty() {
        let conn = setup_test_db();
        let metrics = query_core_metrics(&conn, "test.com", "2024-01-01", "2024-02-01").unwrap();
        assert_eq!(metrics.unique_visitors, 0);
        assert_eq!(metrics.total_pageviews, 0);
        assert!(metrics.pages_per_visit.abs() < f64::EPSILON);
    }

    #[test]
    fn test_core_metrics_with_data() {
        let conn = setup_test_db();
        insert_pageview(&conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&conn, "v1", "2024-01-15 10:05:00", "/about");
        insert_pageview(&conn, "v2", "2024-01-15 11:00:00", "/");

        let metrics = query_core_metrics(&conn, "test.com", "2024-01-01", "2024-02-01").unwrap();
        assert_eq!(metrics.unique_visitors, 2);
        assert_eq!(metrics.total_pageviews, 3);
        assert!((metrics.pages_per_visit - 1.5).abs() < f64::EPSILON);
    }
}
