/// Result of a sequence match query.
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct SequenceMatchResult {
    pub converting_visitors: u64,
    pub total_visitors: u64,
    pub conversion_rate: f64,
}

/// Query user journey patterns using `sequence_match` from the behavioral extension.
///
/// `pattern` is a sequence pattern string like `'(?1).*(?t<=3600)(?2)'`.
/// `conditions` are SQL boolean expressions for each numbered condition.
#[allow(dead_code)]
pub fn build_sequence_match_sql(pattern: &str, conditions: &[&str]) -> String {
    let conds = conditions.join(", ");
    format!(
        "SELECT
            COUNT(*) FILTER (WHERE matched) AS converting_visitors,
            COUNT(*) AS total_visitors,
            COALESCE(COUNT(*) FILTER (WHERE matched)::FLOAT / NULLIF(COUNT(*), 0), 0) AS conversion_rate
         FROM (
             SELECT visitor_id,
                 sequence_match('{pattern}', timestamp, {conds}) AS matched
             FROM events
             WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)
             GROUP BY visitor_id
         )"
    )
}

/// Count sequence occurrences using `sequence_count`.
#[allow(dead_code)]
pub fn build_sequence_count_sql(pattern: &str, conditions: &[&str]) -> String {
    let conds = conditions.join(", ");
    format!(
        "SELECT visitor_id,
             sequence_count('{pattern}', timestamp, {conds}) AS match_count
         FROM events
         WHERE site_id = ? AND timestamp >= CAST(? AS TIMESTAMP) AND timestamp < CAST(? AS TIMESTAMP)
         GROUP BY visitor_id
         ORDER BY match_count DESC"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_sequence_match_sql() {
        let sql = build_sequence_match_sql(
            "(?1).*(?t<=3600)(?2)",
            &["pathname = '/pricing'", "event_name = 'signup'"],
        );
        assert!(sql.contains("sequence_match("));
        assert!(sql.contains("pathname = '/pricing'"));
        assert!(sql.contains("event_name = 'signup'"));
    }

    #[test]
    fn test_build_sequence_count_sql() {
        let sql = build_sequence_count_sql(
            "(?1).*(?2)",
            &["event_name = 'view'", "event_name = 'purchase'"],
        );
        assert!(sql.contains("sequence_count("));
        assert!(sql.contains("match_count"));
    }
}
