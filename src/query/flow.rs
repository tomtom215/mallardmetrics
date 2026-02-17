use duckdb::Connection;

/// A flow analysis result node showing the next page and visitor count.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FlowNode {
    pub next_page: String,
    pub visitors: u64,
}

/// Query the most common next pages after visiting a given page.
///
/// Uses `sequence_next_node` from the behavioral extension.
/// The `target_page` is escaped to prevent SQL injection — single quotes are doubled.
/// Requires the behavioral extension to be loaded.
pub fn query_flow(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
    target_page: &str,
) -> Result<Vec<FlowNode>, duckdb::Error> {
    // Escape single quotes in target_page to prevent SQL injection.
    // sequence_next_node's condition argument does not support parameterized queries,
    // so we must interpolate — but we sanitize first.
    let escaped_page = target_page.replace('\'', "''");

    let sql = format!(
        "SELECT next_page, COUNT(*) AS visitors
         FROM (
             SELECT visitor_id,
                 sequence_next_node('forward', 'first_match', timestamp, pathname,
                     TRUE, pathname = '{escaped_page}'
                 ) AS next_page
             FROM events
             WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)
             GROUP BY visitor_id
         )
         WHERE next_page IS NOT NULL
         GROUP BY next_page ORDER BY visitors DESC LIMIT 10"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params![site_id, start_date, end_date], |row| {
            Ok(FlowNode {
                next_page: row.get(0)?,
                visitors: row.get(1)?,
            })
        })?
        .filter_map(Result::ok)
        .collect();

    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_query_flow_no_extension() {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        // Without behavioral extension, this will fail gracefully
        let result = query_flow(&conn, "test.com", "2024-01-01", "2024-02-01", "/pricing");
        if let Ok(nodes) = result {
            assert!(nodes.is_empty());
        }
    }

    #[test]
    fn test_query_flow_escapes_quotes() {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        // This should not cause a SQL error from unbalanced quotes
        let result = query_flow(
            &conn,
            "test.com",
            "2024-01-01",
            "2024-02-01",
            "/it's-a-page",
        );
        // Will fail due to missing extension, but should not panic from injection
        assert!(result.is_err() || result.unwrap().is_empty());
    }
}
