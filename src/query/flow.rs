/// Build a flow analysis SQL query using `sequence_next_node` from the behavioral extension.
///
/// Finds the most common next pages after a given page.
#[allow(dead_code)]
pub fn build_flow_sql(target_page: &str) -> String {
    format!(
        "SELECT next_page, COUNT(*) AS visitors
         FROM (
             SELECT visitor_id,
                 sequence_next_node('forward', 'first_match', timestamp, pathname,
                     TRUE, pathname = '{target_page}'
                 ) AS next_page
             FROM events
             WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)
             GROUP BY visitor_id
         )
         WHERE next_page IS NOT NULL
         GROUP BY next_page ORDER BY visitors DESC LIMIT 10"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_flow_sql() {
        let sql = build_flow_sql("/pricing");
        assert!(sql.contains("sequence_next_node("));
        assert!(sql.contains("pathname = '/pricing'"));
        assert!(sql.contains("LIMIT 10"));
    }
}
