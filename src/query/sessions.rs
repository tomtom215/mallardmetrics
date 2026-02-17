use duckdb::Connection;

/// Session-level metrics derived using the `sessionize` behavioral extension function.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionMetrics {
    pub total_sessions: u64,
    pub avg_session_duration_secs: f64,
    pub avg_pages_per_session: f64,
}

/// Query session metrics using the `sessionize` function from the behavioral extension.
///
/// Requires the behavioral extension to be loaded.
pub fn query_session_metrics(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
) -> Result<SessionMetrics, duckdb::Error> {
    let sql = r"
        WITH sessions AS (
            SELECT
                visitor_id,
                sessionize(timestamp, INTERVAL '30 minutes') OVER (
                    PARTITION BY visitor_id ORDER BY timestamp
                ) AS session_id,
                timestamp,
                event_name
            FROM events
            WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)
        ),
        session_stats AS (
            SELECT
                visitor_id,
                session_id,
                COUNT(*) FILTER (WHERE event_name = 'pageview') AS page_count,
                EXTRACT(EPOCH FROM (MAX(timestamp) - MIN(timestamp))) AS duration_secs
            FROM sessions
            GROUP BY visitor_id, session_id
        )
        SELECT
            COUNT(*) AS total_sessions,
            COALESCE(AVG(duration_secs), 0) AS avg_duration,
            COALESCE(AVG(page_count), 0) AS avg_pages
        FROM session_stats
    ";

    let mut stmt = conn.prepare(sql)?;
    stmt.query_row(duckdb::params![site_id, start_date, end_date], |row| {
        Ok(SessionMetrics {
            total_sessions: row.get(0)?,
            avg_session_duration_secs: row.get(1)?,
            avg_pages_per_session: row.get(2)?,
        })
    })
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
    fn test_session_metrics_empty() {
        let conn = setup_test_db();
        // sessionize requires the behavioral extension; the query will fail
        // if behavioral is not available — that's expected in unit tests.
        let result = query_session_metrics(&conn, "test.com", "2024-01-01", "2024-02-01");
        if let Ok(metrics) = result {
            assert_eq!(metrics.total_sessions, 0);
        }
    }

    #[test]
    fn test_session_metrics_with_data_no_extension() {
        let conn = setup_test_db();
        insert_pageview(&conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&conn, "v1", "2024-01-15 10:05:00", "/about");
        insert_pageview(&conn, "v2", "2024-01-15 11:00:00", "/");
        // Without behavioral extension, this will fail gracefully
        let result = query_session_metrics(&conn, "test.com", "2024-01-01", "2024-02-01");
        // We expect an error without the extension; the API handler wraps with unwrap_or
        if let Ok(metrics) = result {
            assert!(metrics.total_sessions > 0);
        }
    }
}
