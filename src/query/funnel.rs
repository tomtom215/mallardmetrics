use duckdb::Connection;

/// Funnel step result showing how many visitors reached each step.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FunnelStep {
    pub step: u32,
    pub visitors: u64,
}

/// Build and execute a funnel query using `window_funnel` from the behavioral extension.
///
/// `steps` defines the funnel conditions as SQL boolean expressions.
/// `window_interval` is the maximum time between first and last step (e.g., "1 day").
pub fn query_funnel(
    conn: &Connection,
    site_id: &str,
    start_date: &str,
    end_date: &str,
    window_interval: &str,
    steps: &[&str],
) -> Result<Vec<FunnelStep>, duckdb::Error> {
    if steps.is_empty() {
        return Ok(Vec::new());
    }

    let step_conditions = steps.join(", ");

    // Note: step conditions are SQL expressions defined by the application,
    // not user input. User-provided values (site_id, dates) are parameterized.
    let sql = format!(
        "SELECT steps, COUNT(*) AS visitors
         FROM (
             SELECT visitor_id,
                 window_funnel(INTERVAL '{window_interval}', timestamp,
                     {step_conditions}
                 ) AS steps
             FROM events
             WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)
             GROUP BY visitor_id
         )
         GROUP BY steps ORDER BY steps"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(duckdb::params![site_id, start_date, end_date], |row| {
            Ok(FunnelStep {
                step: row.get(0)?,
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
    fn test_funnel_empty_steps() {
        let conn = Connection::open_in_memory().unwrap();
        crate::storage::schema::init_schema(&conn).unwrap();

        let result =
            query_funnel(&conn, "test.com", "2024-01-01", "2024-02-01", "1 day", &[]).unwrap();
        assert!(result.is_empty());
    }
}
