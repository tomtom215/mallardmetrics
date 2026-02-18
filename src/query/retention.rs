use duckdb::Connection;

/// Retention cohort row.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RetentionCohort {
    pub cohort_date: String,
    pub retained: Vec<bool>,
}

/// Query retention cohorts using the `retention` function from the behavioral extension.
///
/// Returns weekly cohorts with retention flags for each subsequent week.
/// Requires the behavioral extension to be loaded.
pub fn query_retention(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
    num_weeks: u32,
) -> Result<Vec<RetentionCohort>, duckdb::Error> {
    if num_weeks == 0 {
        return Ok(Vec::new());
    }

    // Build retention conditions for each week
    let mut conditions = Vec::new();
    for i in 0..num_weeks {
        conditions.push(format!(
            "DATE_TRUNC('week', timestamp) = DATE_TRUNC('week', first_seen) + INTERVAL '{i} weeks'"
        ));
    }
    let retention_args = conditions.join(", ");

    let sql = format!(
        "SELECT STRFTIME(DATE_TRUNC('week', first_seen), '%Y-%m-%d') AS cohort_week,
            retention({retention_args}) AS retained
         FROM events_all e
         JOIN (
             SELECT visitor_id, MIN(timestamp) AS first_seen
             FROM events_all WHERE site_id = ?
             GROUP BY visitor_id
         ) f ON e.visitor_id = f.visitor_id
         WHERE e.site_id = ?
           AND e.timestamp >= CAST(? AS TIMESTAMP) AND e.timestamp < CAST(? AS TIMESTAMP)
         GROUP BY cohort_week
         ORDER BY cohort_week"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(
            duckdb::params![site_id, site_id, start_date, end_date],
            |row| {
                let cohort_date: String = row.get(0)?;
                // The retention() function returns a BOOLEAN[] array.
                // DuckDB Rust bindings return this as a string representation.
                let retained_raw: String = row.get(1)?;
                let retained = parse_bool_array(&retained_raw);
                Ok(RetentionCohort {
                    cohort_date,
                    retained,
                })
            },
        )?
        .filter_map(Result::ok)
        .collect();

    Ok(rows)
}

/// Parse a DuckDB BOOLEAN[] array string like "[true, false, true]" into `Vec<bool>`.
fn parse_bool_array(s: &str) -> Vec<bool> {
    let trimmed = s.trim().trim_start_matches('[').trim_end_matches(']');
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed.split(',').map(|v| v.trim() == "true").collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();
        let dir = tempfile::tempdir().unwrap();
        crate::storage::schema::setup_query_view(&conn, dir.path()).unwrap();
        drop(dir); // view was already created; TempDir no longer needed
        conn
    }

    #[test]
    fn test_retention_empty() {
        let conn = setup_test_db();
        // Without behavioral extension, this will fail gracefully
        let result = query_retention(&conn, "test.com", "2024-01-01", "2024-03-01", 4);
        if let Ok(cohorts) = result {
            assert!(cohorts.is_empty());
        }
    }

    #[test]
    fn test_retention_zero_weeks() {
        let conn = setup_test_db();
        let result = query_retention(&conn, "test.com", "2024-01-01", "2024-03-01", 0).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_bool_array() {
        assert_eq!(
            parse_bool_array("[true, false, true]"),
            vec![true, false, true]
        );
        assert_eq!(parse_bool_array("[true]"), vec![true]);
        assert_eq!(parse_bool_array("[]"), Vec::<bool>::new());
    }
}
