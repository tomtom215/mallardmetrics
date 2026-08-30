use super::QueryScope;
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// Minimum and maximum cohort periods.
///
/// The behavioral extension's `retention()` accepts 2–32 boolean conditions.
/// The API previously advertised 1–52 weeks, so `weeks=1` and `weeks>32` both
/// produced a binder error that was swallowed and reported as "no data".
pub const MIN_PERIODS: u32 = 2;
pub const MAX_PERIODS: u32 = 32;

/// One cohort row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionCohort {
    /// First day of the cohort's first week, `YYYY-MM-DD`.
    pub cohort_date: String,
    /// Visitors first seen during the cohort week.
    pub cohort_size: u64,
    /// Retained visitor counts, index 0 being the cohort week itself.
    pub retained: Vec<u64>,
    /// `retained[i] / cohort_size`, 0.0–1.0.
    pub retention_rates: Vec<f64>,
}

/// Whether the configured visitor-ID rotation can support `weeks` of cohorts.
///
/// `visitor_id` is an HMAC over a rotating salt. Once the salt rotates, the same
/// person is a different visitor. With the default one-day rotation, no visitor
/// can ever appear in week 1 of their own cohort, so every retention figure past
/// week 0 is structurally zero — not a property of the site's audience.
pub const fn rotation_supports_weeks(rotation_days: u32, weeks: u32) -> bool {
    // Week N needs identities to survive at least 7*N days.
    rotation_days as u64 >= (weeks.saturating_sub(1) as u64) * 7
}

/// Query weekly retention cohorts.
///
/// Returns per-visitor retention counts. The previous implementation grouped
/// only by cohort week, so `retention()` aggregated every visitor in the cohort
/// into a single boolean array: an entry was `true` if *any one* visitor
/// returned that week. That is a "did anybody come back" flag, not retention,
/// and for any active site it was `true` almost everywhere.
///
/// # Errors
///
/// Returns an error if the query fails, e.g. when the behavioral extension is
/// not loaded.
pub fn query_retention(
    conn: &Connection,
    scope: &QueryScope,
    weeks: u32,
) -> Result<Vec<RetentionCohort>, duckdb::Error> {
    if !(MIN_PERIODS..=MAX_PERIODS).contains(&weeks) {
        return Ok(Vec::new());
    }

    let sql = retention_sql(weeks);
    let mut stmt = conn.prepare(&sql)?;
    let weeks_usize = weeks as usize;

    // Bind order: end (for the first-seen bound), start, site, then site,
    // start, end for the event scan.
    let rows = stmt
        .query_map(
            duckdb::params![
                scope.site_id,
                scope.end,
                scope.start,
                scope.site_id,
                scope.start,
                scope.end
            ],
            |row| {
                let cohort_date: String = row.get(0)?;
                let mut retained = Vec::with_capacity(weeks_usize);
                for i in 0..weeks_usize {
                    retained.push(row.get::<_, u64>(i + 1)?);
                }
                Ok((cohort_date, retained))
            },
        )?
        .filter_map(Result::ok)
        .map(|(cohort_date, retained)| {
            let cohort_size = retained.first().copied().unwrap_or(0);
            #[allow(clippy::cast_precision_loss)]
            let retention_rates = retained
                .iter()
                .map(|n| {
                    if cohort_size > 0 {
                        *n as f64 / cohort_size as f64
                    } else {
                        0.0
                    }
                })
                .collect();
            RetentionCohort {
                cohort_date,
                cohort_size,
                retained,
                retention_rates,
            }
        })
        .collect();

    Ok(rows)
}

/// SQL for the per-visitor retention report. Split out so it can be unit-tested.
fn retention_sql(weeks: u32) -> String {
    let conditions: Vec<String> = (0..weeks)
        .map(|i| {
            format!(
                "DATE_TRUNC('week', e.timestamp) = DATE_TRUNC('week', f.first_ts) \
                 + INTERVAL '{i} weeks'"
            )
        })
        .collect();
    let retention_args = conditions.join(",\n                 ");

    // DuckDB list indices are 1-based, so r[i+1] is week i.
    let count_columns: Vec<String> = (0..weeks)
        .map(|i| format!("COUNT(*) FILTER (WHERE r[{}]) AS w{i}", i + 1))
        .collect();
    let counts = count_columns.join(",\n                ");

    // `first_seen` is bounded above by the query's end so it is not a full-history
    // scan, and filtered below by the query's start so only cohorts that actually
    // formed inside the window are reported. Without the lower bound, visitors
    // whose first visit predates the window produce cohorts with size 0 and
    // non-zero later weeks.
    format!(
        "WITH first_seen AS (
             SELECT visitor_id, MIN(timestamp) AS first_ts
             FROM events_all
             WHERE site_id = ? AND timestamp < CAST(? AS TIMESTAMP)
             GROUP BY visitor_id
             HAVING MIN(timestamp) >= CAST(? AS TIMESTAMP)
         ),
         per_visitor AS (
             SELECT DATE_TRUNC('week', f.first_ts) AS cohort_week,
                    e.visitor_id AS visitor_id,
                    retention(
                 {retention_args}
                    ) AS r
             FROM events_all e
             JOIN first_seen f ON e.visitor_id = f.visitor_id
             WHERE e.site_id = ?
               AND e.timestamp >= CAST(? AS TIMESTAMP)
               AND e.timestamp <  CAST(? AS TIMESTAMP)
             GROUP BY cohort_week, e.visitor_id
         )
         SELECT STRFTIME(cohort_week, '%Y-%m-%d') AS cohort_date,
                {counts}
         FROM per_visitor
         GROUP BY cohort_week
         ORDER BY cohort_week"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, insert_pageview, scope};

    /// A four-visitor cohort in the week of 2024-01-01:
    /// r1 returns in weeks 1 and 2, r2 in week 1, r3 and r4 never.
    fn seed(db: &TestDb) {
        insert_pageview(&db.conn, "r1", "2024-01-02 10:00:00", "/");
        insert_pageview(&db.conn, "r1", "2024-01-09 10:00:00", "/");
        insert_pageview(&db.conn, "r1", "2024-01-16 10:00:00", "/");
        insert_pageview(&db.conn, "r2", "2024-01-03 10:00:00", "/");
        insert_pageview(&db.conn, "r2", "2024-01-10 10:00:00", "/");
        insert_pageview(&db.conn, "r3", "2024-01-04 10:00:00", "/");
        insert_pageview(&db.conn, "r4", "2024-01-05 10:00:00", "/");
    }

    #[test]
    fn test_retention_counts_visitors_not_a_boolean_flag() {
        // Regression: grouping only by cohort week made retention() aggregate
        // across the whole cohort, so the result was [true, true, true] — a
        // "did anyone come back" flag rather than how many did.
        let db = TestDb::new();
        if !db.require_behavioral("retention cohorts") {
            return;
        }
        seed(&db);

        let cohorts = query_retention(&db.conn, &scope("2024-01-01", "2024-03-01"), 4).unwrap();

        assert_eq!(cohorts.len(), 1);
        let c = &cohorts[0];
        assert_eq!(c.cohort_date, "2024-01-01");
        assert_eq!(c.cohort_size, 4);
        assert_eq!(c.retained, vec![4, 2, 1, 0]);
    }

    #[test]
    fn test_retention_rates() {
        let db = TestDb::new();
        if !db.require_behavioral("retention cohorts") {
            return;
        }
        seed(&db);
        let cohorts = query_retention(&db.conn, &scope("2024-01-01", "2024-03-01"), 4).unwrap();
        let rates = &cohorts[0].retention_rates;
        assert!((rates[0] - 1.0).abs() < f64::EPSILON);
        assert!((rates[1] - 0.5).abs() < f64::EPSILON);
        assert!((rates[2] - 0.25).abs() < f64::EPSILON);
        assert!(rates[3].abs() < f64::EPSILON);
    }

    #[test]
    fn test_cohorts_forming_before_the_window_are_excluded() {
        let db = TestDb::new();
        if !db.require_behavioral("retention cohorts") {
            return;
        }
        // First seen well before the window; returns inside it.
        insert_pageview(&db.conn, "old", "2023-11-01 10:00:00", "/");
        insert_pageview(&db.conn, "old", "2024-01-10 10:00:00", "/");
        seed(&db);

        let cohorts = query_retention(&db.conn, &scope("2024-01-01", "2024-03-01"), 4).unwrap();
        assert_eq!(cohorts.len(), 1, "only the in-window cohort is reported");
        assert_eq!(cohorts[0].cohort_size, 4);
    }

    #[test]
    fn test_retention_is_monotonically_non_increasing_within_a_cohort() {
        let db = TestDb::new();
        if !db.require_behavioral("retention cohorts") {
            return;
        }
        seed(&db);
        let cohorts = query_retention(&db.conn, &scope("2024-01-01", "2024-03-01"), 4).unwrap();
        let retained = &cohorts[0].retained;
        assert!(
            retained.iter().all(|n| *n <= retained[0]),
            "no week can exceed the cohort size: {retained:?}"
        );
    }

    #[test]
    fn test_empty_range_returns_no_cohorts() {
        let db = TestDb::new();
        if !db.require_behavioral("retention cohorts") {
            return;
        }
        let cohorts = query_retention(&db.conn, &scope("2024-01-01", "2024-03-01"), 4).unwrap();
        assert!(cohorts.is_empty());
    }

    #[test]
    fn test_period_bounds_are_rejected_not_errored() {
        // The extension accepts 2..=32 conditions; the API used to advertise
        // 1..=52, so both ends produced a binder error reported as "no data".
        let db = TestDb::new();
        let s = scope("2024-01-01", "2024-03-01");
        assert!(query_retention(&db.conn, &s, 0).unwrap().is_empty());
        assert!(query_retention(&db.conn, &s, 1).unwrap().is_empty());
        assert!(query_retention(&db.conn, &s, 33).unwrap().is_empty());
        assert!(query_retention(&db.conn, &s, 52).unwrap().is_empty());
    }

    #[test]
    fn test_max_periods_is_accepted_by_the_extension() {
        let db = TestDb::new();
        if !db.require_behavioral("retention cohorts") {
            return;
        }
        seed(&db);
        let cohorts =
            query_retention(&db.conn, &scope("2024-01-01", "2024-12-01"), MAX_PERIODS).unwrap();
        assert_eq!(cohorts[0].retained.len(), MAX_PERIODS as usize);
    }

    #[test]
    fn test_rotation_support_check() {
        // A daily-rotating salt cannot support even week 1.
        assert!(!rotation_supports_weeks(1, 2));
        assert!(rotation_supports_weeks(1, 1));
        assert!(rotation_supports_weeks(7, 2));
        assert!(!rotation_supports_weeks(7, 3));
        assert!(rotation_supports_weeks(30, 4));
        assert!(!rotation_supports_weeks(30, 6));
    }

    #[test]
    fn test_sql_generates_one_condition_and_one_column_per_week() {
        let sql = retention_sql(3);
        assert_eq!(sql.matches("INTERVAL '").count(), 3);
        assert!(sql.contains("r[1]") && sql.contains("r[2]") && sql.contains("r[3]"));
        assert!(!sql.contains("r[0]"), "DuckDB list indices are 1-based");
        assert!(!sql.contains("r[4]"));
    }

    #[test]
    fn test_sql_bounds_the_first_seen_scan() {
        // Without the upper bound this scanned the site's entire history on
        // every request.
        let sql = retention_sql(4);
        assert!(sql.contains("timestamp < CAST(? AS TIMESTAMP)"));
        assert!(sql.contains("HAVING MIN(timestamp) >= CAST(? AS TIMESTAMP)"));
    }

    #[test]
    fn test_sql_uses_bound_parameters() {
        let sql = retention_sql(4);
        assert_eq!(sql.matches('?').count(), 6);
    }
}
