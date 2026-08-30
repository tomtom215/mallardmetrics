use super::QueryScope;
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// Result of a sequence-match query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SequenceMatchResult {
    /// Visitors whose event stream matched the pattern.
    pub converting_visitors: u64,
    /// Visitors in range.
    pub total_visitors: u64,
    /// `converting_visitors / total_visitors`, 0.0–1.0.
    pub conversion_rate: f64,
    /// Total non-overlapping matches across all visitors.
    ///
    /// A visitor who completes the sequence three times contributes 1 to
    /// `converting_visitors` and 3 here.
    pub total_matches: u64,
}

/// The extension accepts 2–32 boolean conditions.
pub const MIN_CONDITIONS: usize = 2;
pub const MAX_CONDITIONS: usize = 32;

/// Build a `sequence_match` pattern: `(?1).*(?2).*…(?N)`.
fn build_pattern(num_conditions: usize) -> String {
    (1..=num_conditions)
        .map(|i| format!("(?{i})"))
        .collect::<Vec<_>>()
        .join(".*")
}

/// SQL for a sequence-match query. Split out so it can be unit-tested.
///
/// `conditions` are SQL boolean expressions built by the API layer from
/// validated step specifications, never raw request input.
fn build_sequence_match_sql(scope: &QueryScope, conditions: &[&str]) -> String {
    let pattern = build_pattern(conditions.len());
    let conds = conditions.join(", ");
    format!(
        "SELECT
            COUNT(*) FILTER (WHERE matched) AS converting_visitors,
            COUNT(*) AS total_visitors,
            COALESCE(COUNT(*) FILTER (WHERE matched)::DOUBLE / NULLIF(COUNT(*), 0), 0.0)
                AS conversion_rate,
            COALESCE(SUM(match_count), 0) AS total_matches
         FROM (
             SELECT visitor_id,
                 sequence_match('{pattern}', timestamp, {conds}) AS matched,
                 sequence_count('{pattern}', timestamp, {conds}) AS match_count
             FROM events_all
             WHERE {where_clause}
             GROUP BY visitor_id
         )",
        where_clause = scope.where_clause()
    )
}

/// Execute a sequence-match query.
///
/// # Errors
///
/// Returns an error if the query fails, e.g. when the behavioral extension is
/// not loaded.
pub fn execute_sequence_match(
    conn: &Connection,
    scope: &QueryScope,
    conditions: &[&str],
) -> Result<SequenceMatchResult, duckdb::Error> {
    if !(MIN_CONDITIONS..=MAX_CONDITIONS).contains(&conditions.len()) {
        return Ok(SequenceMatchResult {
            converting_visitors: 0,
            total_visitors: 0,
            conversion_rate: 0.0,
            total_matches: 0,
        });
    }

    let sql = build_sequence_match_sql(scope, conditions);
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(duckdb::params_from_iter(scope.params()), |row| {
        Ok(SequenceMatchResult {
            converting_visitors: row.get(0)?,
            total_visitors: row.get(1)?,
            conversion_rate: row.get(2)?,
            total_matches: row.get(3)?,
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, insert_pageview, scope};

    const CONDITIONS: &[&str] = &["pathname = '/'", "event_name = 'signup'"];

    #[test]
    fn test_build_pattern() {
        assert_eq!(build_pattern(1), "(?1)");
        assert_eq!(build_pattern(2), "(?1).*(?2)");
        assert_eq!(build_pattern(3), "(?1).*(?2).*(?3)");
    }

    #[test]
    fn test_build_sequence_match_sql() {
        let sql = build_sequence_match_sql(&scope("2024-01-01", "2024-02-01"), CONDITIONS);
        assert!(sql.contains("sequence_match("));
        assert!(sql.contains("sequence_count("));
        assert!(sql.contains("(?1).*(?2)"));
        assert!(sql.contains("pathname = '/'"));
        assert_eq!(sql.matches('?').count(), 3 + 4, "3 binds + 4 pattern refs");
    }

    #[test]
    fn test_sequence_match_counts_converting_visitors() {
        let db = TestDb::new();
        if !db.require_behavioral("sequence analysis") {
            return;
        }
        // v1 completes the sequence; v2 only lands on the first page.
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        db.insert_event("v1", "2024-01-15 10:05:00", "signup", "/");
        insert_pageview(&db.conn, "v2", "2024-01-15 11:00:00", "/");

        let result =
            execute_sequence_match(&db.conn, &scope("2024-01-01", "2024-02-01"), CONDITIONS)
                .unwrap();
        assert_eq!(result.converting_visitors, 1);
        assert_eq!(result.total_visitors, 2);
        assert!((result.conversion_rate - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_sequence_match_counts_repeat_completions() {
        let db = TestDb::new();
        if !db.require_behavioral("sequence analysis") {
            return;
        }
        // One visitor completes the sequence twice.
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        db.insert_event("v1", "2024-01-15 10:05:00", "signup", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 11:00:00", "/");
        db.insert_event("v1", "2024-01-15 11:05:00", "signup", "/");

        let result =
            execute_sequence_match(&db.conn, &scope("2024-01-01", "2024-02-01"), CONDITIONS)
                .unwrap();
        assert_eq!(result.converting_visitors, 1);
        assert_eq!(result.total_matches, 2, "both completions are counted");
    }

    #[test]
    fn test_order_matters() {
        let db = TestDb::new();
        if !db.require_behavioral("sequence analysis") {
            return;
        }
        // signup happens before the pageview, so the pattern must not match.
        db.insert_event("v1", "2024-01-15 10:00:00", "signup", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:05:00", "/");

        let result =
            execute_sequence_match(&db.conn, &scope("2024-01-01", "2024-02-01"), CONDITIONS)
                .unwrap();
        assert_eq!(result.converting_visitors, 0);
    }

    #[test]
    fn test_empty_range() {
        let db = TestDb::new();
        if !db.require_behavioral("sequence analysis") {
            return;
        }
        let result =
            execute_sequence_match(&db.conn, &scope("2024-01-01", "2024-02-01"), CONDITIONS)
                .unwrap();
        assert_eq!(result.total_visitors, 0);
        assert!(result.conversion_rate.abs() < f64::EPSILON);
    }

    #[test]
    fn test_condition_count_bounds_are_rejected_not_errored() {
        let db = TestDb::new();
        let s = scope("2024-01-01", "2024-02-01");
        assert_eq!(
            execute_sequence_match(&db.conn, &s, &[])
                .unwrap()
                .total_visitors,
            0
        );
        assert_eq!(
            execute_sequence_match(&db.conn, &s, &["pathname = '/'"])
                .unwrap()
                .total_visitors,
            0
        );
        let many: Vec<&str> = vec!["pathname = '/'"; MAX_CONDITIONS + 1];
        assert_eq!(
            execute_sequence_match(&db.conn, &s, &many)
                .unwrap()
                .total_visitors,
            0
        );
    }
}
