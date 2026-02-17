use duckdb::Connection;

/// Retention cohort row.
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct RetentionCohort {
    pub cohort_date: String,
    pub retained: Vec<bool>,
}

/// Query retention cohorts using the `retention` function from the behavioral extension.
///
/// Returns weekly cohorts with retention flags for each subsequent week.
#[allow(dead_code)]
pub fn query_retention(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
    num_weeks: u32,
) -> String {
    // Build retention conditions for each week
    let mut conditions = Vec::new();
    for i in 0..num_weeks {
        conditions.push(format!(
            "DATE_TRUNC('week', timestamp) = DATE_TRUNC('week', first_seen) + INTERVAL '{i} weeks'"
        ));
    }
    let retention_args = conditions.join(", ");

    let sql = format!(
        "SELECT DATE_TRUNC('week', first_seen) AS cohort_week,
            retention({retention_args}) AS retained
         FROM events e
         JOIN (
             SELECT visitor_id, MIN(timestamp) AS first_seen
             FROM events WHERE site_id = ?
             GROUP BY visitor_id
         ) f ON e.visitor_id = f.visitor_id
         WHERE e.site_id = ?
           AND e.timestamp >= CAST(? AS TIMESTAMP) AND e.timestamp < CAST(? AS TIMESTAMP)
         GROUP BY cohort_week
         ORDER BY cohort_week"
    );

    // Return the SQL for now; execution requires the behavioral extension
    let _ = conn;
    let _ = site_id;
    let _ = start_date;
    let _ = end_date;
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_retention_sql_generation() {
        let conn = Connection::open_in_memory().unwrap();
        let sql = query_retention(&conn, "test.com", "2024-01-01", "2024-03-01", 4);
        assert!(sql.contains("retention("));
        assert!(sql.contains("INTERVAL '0 weeks'"));
        assert!(sql.contains("INTERVAL '3 weeks'"));
    }
}
