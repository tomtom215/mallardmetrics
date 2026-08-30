use super::QueryScope;
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// Maximum funnel steps.
///
/// The behavioral extension accepts 2–32 boolean conditions, matching
/// ClickHouse. Asking for more produces a binder error rather than a result.
pub const MAX_STEPS: usize = 32;
/// Minimum funnel steps accepted by `window_funnel`.
pub const MIN_STEPS: usize = 2;

/// One row of a funnel report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunnelStep {
    /// 1-based step index.
    pub step: u32,
    /// Visitors who reached *at least* this step.
    pub visitors: u64,
    /// `visitors / visitors_at_step_1`, 0.0–1.0. 0.0 when step 1 had nobody.
    pub conversion_rate: f64,
    /// Visitors lost between the previous step and this one.
    pub dropped_off: u64,
}

/// Combinable `window_funnel` modes.
///
/// Passed through to the extension verbatim after validation; an unknown mode
/// would otherwise become a SQL binder error surfaced as a 500.
pub const VALID_MODES: &[&str] = &[
    "strict",
    "strict_deduplication",
    "strict_order",
    "strict_increase",
    "strict_once",
    "allow_reentry",
    "timestamp_dedup",
];

/// Validate a comma-separated mode list, returning the normalised form.
///
/// Returns `None` if any mode is unrecognised.
pub fn normalize_modes(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Some(String::new());
    }
    let mut modes = Vec::new();
    for part in trimmed.split(',') {
        let mode = part.trim().to_ascii_lowercase();
        if !VALID_MODES.contains(&mode.as_str()) {
            return None;
        }
        if !modes.contains(&mode) {
            modes.push(mode);
        }
    }
    Some(modes.join(", "))
}

/// Run a funnel query.
///
/// `steps` are SQL boolean expressions built by the API layer from validated
/// `page:` / `event:` specifications — never raw user input.
///
/// Returns one row per step with **cumulative** visitor counts: the number who
/// reached at least that step. `window_funnel` returns the furthest step each
/// visitor reached, so grouping by that value directly (as the previous release
/// did) reports "visitors who stopped at exactly step N" while labelling it as
/// the funnel. That inverts the shape of the report — a visitor who converted
/// all the way through was not counted at step 1 at all — and the dashboard
/// then normalised the bars against the largest of those counts, so the
/// percentages were wrong too.
///
/// Steps with no visitors are still returned, so the report always has one row
/// per step rather than silently shortening.
///
/// # Errors
///
/// Returns an error if the query fails, e.g. when the behavioral extension is
/// not loaded.
pub fn query_funnel(
    conn: &Connection,
    scope: &QueryScope,
    window_interval: &str,
    modes: &str,
    steps: &[&str],
) -> Result<Vec<FunnelStep>, duckdb::Error> {
    if steps.len() < MIN_STEPS || steps.len() > MAX_STEPS {
        return Ok(Vec::new());
    }

    let sql = funnel_sql(window_interval, modes, steps);
    let mut stmt = conn.prepare(&sql)?;
    let raw: Vec<(u32, u64)> = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            let step: i64 = row.get(0)?;
            let visitors: u64 = row.get(1)?;
            Ok((u32::try_from(step).unwrap_or(0), visitors))
        })?
        .filter_map(Result::ok)
        .collect();

    Ok(to_report(&raw))
}

/// Turn `(step, cumulative_visitors)` pairs into a full report.
fn to_report(raw: &[(u32, u64)]) -> Vec<FunnelStep> {
    let first = raw.first().map_or(0, |(_, v)| *v);
    let mut previous = first;
    raw.iter()
        .enumerate()
        .map(|(i, (step, visitors))| {
            #[allow(clippy::cast_precision_loss)]
            let conversion_rate = if first > 0 {
                *visitors as f64 / first as f64
            } else {
                0.0
            };
            let dropped_off = if i == 0 {
                0
            } else {
                previous.saturating_sub(*visitors)
            };
            previous = *visitors;
            FunnelStep {
                step: *step,
                visitors: *visitors,
                conversion_rate,
                dropped_off,
            }
        })
        .collect()
}

/// SQL for the cumulative funnel. Split out so it can be unit-tested.
fn funnel_sql(window_interval: &str, modes: &str, steps: &[&str]) -> String {
    let step_conditions = steps.join(", ");
    let n = steps.len();
    // `window_funnel(window [, mode], timestamp, cond, ...)`: the mode argument
    // is positional and must be omitted entirely when empty.
    let mode_arg = if modes.trim().is_empty() {
        String::new()
    } else {
        format!("'{}', ", modes.replace('\'', "''"))
    };

    format!(
        "WITH per_visitor AS (
             SELECT visitor_id,
                 window_funnel(INTERVAL '{window_interval}', {mode_arg}timestamp,
                     {step_conditions}
                 ) AS furthest
             FROM events_all
             WHERE {where_clause}
             GROUP BY visitor_id
         ),
         step_numbers AS (SELECT UNNEST(generate_series(1, {n})) AS step)
         SELECT s.step,
                COUNT(pv.visitor_id) FILTER (WHERE pv.furthest >= s.step) AS visitors
         FROM step_numbers s
         LEFT JOIN per_visitor pv ON TRUE
         GROUP BY s.step
         ORDER BY s.step",
        where_clause = QueryScope::where_clause()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, insert_pageview, scope};

    /// v1 reaches all three steps, v2 two, v3 and v4 only the first.
    fn seed(db: &TestDb) {
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:05:00", "/pricing");
        db.insert_event("v1", "2024-01-15 10:10:00", "signup", "/pricing");
        insert_pageview(&db.conn, "v2", "2024-01-15 11:00:00", "/");
        insert_pageview(&db.conn, "v2", "2024-01-15 11:05:00", "/pricing");
        insert_pageview(&db.conn, "v3", "2024-01-15 12:00:00", "/");
        insert_pageview(&db.conn, "v4", "2024-01-15 13:00:00", "/");
    }

    const STEPS: &[&str] = &[
        "pathname = '/'",
        "pathname = '/pricing'",
        "event_name = 'signup'",
    ];

    #[test]
    fn test_funnel_is_cumulative() {
        // Regression: the report used to group by the furthest step reached, so
        // this dataset produced 2/1/1 ("stopped at exactly step N") instead of
        // the true funnel shape 4/2/1 ("reached at least step N").
        let db = TestDb::new();
        if !db.require_behavioral("funnel analysis") {
            return;
        }
        seed(&db);

        let rows = query_funnel(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "1 day",
            "",
            STEPS,
        )
        .unwrap();

        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].step, rows[0].visitors), (1, 4));
        assert_eq!((rows[1].step, rows[1].visitors), (2, 2));
        assert_eq!((rows[2].step, rows[2].visitors), (3, 1));
    }

    #[test]
    fn test_funnel_is_monotonically_non_increasing() {
        let db = TestDb::new();
        if !db.require_behavioral("funnel analysis") {
            return;
        }
        seed(&db);
        let rows = query_funnel(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "1 day",
            "",
            STEPS,
        )
        .unwrap();
        for pair in rows.windows(2) {
            assert!(
                pair[0].visitors >= pair[1].visitors,
                "a cumulative funnel can never widen: {pair:?}"
            );
        }
    }

    #[test]
    fn test_conversion_rates_and_dropoff() {
        let db = TestDb::new();
        if !db.require_behavioral("funnel analysis") {
            return;
        }
        seed(&db);
        let rows = query_funnel(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "1 day",
            "",
            STEPS,
        )
        .unwrap();

        assert!((rows[0].conversion_rate - 1.0).abs() < f64::EPSILON);
        assert!((rows[1].conversion_rate - 0.5).abs() < f64::EPSILON);
        assert!((rows[2].conversion_rate - 0.25).abs() < f64::EPSILON);
        assert_eq!(rows[0].dropped_off, 0);
        assert_eq!(rows[1].dropped_off, 2);
        assert_eq!(rows[2].dropped_off, 1);
    }

    #[test]
    fn test_funnel_returns_one_row_per_step_when_empty() {
        // An empty funnel must still describe its shape, so the dashboard can
        // render zeroed bars instead of "no data".
        let db = TestDb::new();
        if !db.require_behavioral("funnel analysis") {
            return;
        }
        let rows = query_funnel(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "1 day",
            "",
            STEPS,
        )
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.visitors == 0));
        assert!(rows.iter().all(|r| r.conversion_rate == 0.0));
    }

    #[test]
    fn test_strict_order_mode_is_applied() {
        let db = TestDb::new();
        if !db.require_behavioral("funnel modes") {
            return;
        }
        seed(&db);
        // v1's signup event also satisfies step 2 (same pathname), so under
        // strict_order the chain cannot advance to step 3.
        let rows = query_funnel(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "1 day",
            "strict_order",
            STEPS,
        )
        .unwrap();
        assert_eq!(rows[0].visitors, 4);
        assert_eq!(rows[1].visitors, 2);
        assert_eq!(rows[2].visitors, 0, "strict_order must break v1's chain");
    }

    #[test]
    fn test_too_few_or_too_many_steps_return_nothing() {
        let db = TestDb::new();
        let s = scope("2024-01-01", "2024-02-01");
        assert!(
            query_funnel(&db.conn, &s, "1 day", "", &["pathname = '/'"])
                .unwrap()
                .is_empty()
        );
        let many: Vec<&str> = vec!["pathname = '/'"; MAX_STEPS + 1];
        assert!(
            query_funnel(&db.conn, &s, "1 day", "", &many)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_mode_validation() {
        assert_eq!(normalize_modes(""), Some(String::new()));
        assert_eq!(
            normalize_modes("strict_order"),
            Some("strict_order".to_string())
        );
        assert_eq!(
            normalize_modes(" strict_order , STRICT_ONCE "),
            Some("strict_order, strict_once".to_string())
        );
        assert_eq!(
            normalize_modes("strict_order,strict_order"),
            Some("strict_order".to_string()),
            "duplicates are collapsed"
        );
        assert!(normalize_modes("drop_tables").is_none());
        assert!(normalize_modes("strict_order; DROP TABLE events").is_none());
    }

    #[test]
    fn test_sql_omits_the_mode_argument_when_empty() {
        // window_funnel's mode is positional; passing an empty string would be
        // read as the timestamp argument.
        let sql = funnel_sql("1 day", "", STEPS);
        assert!(sql.contains("window_funnel(INTERVAL '1 day', timestamp,"));
        let with_mode = funnel_sql("1 day", "strict_order", STEPS);
        assert!(with_mode.contains("window_funnel(INTERVAL '1 day', 'strict_order', timestamp,"));
    }

    #[test]
    fn test_sql_uses_bound_parameters_for_scope() {
        let sql = funnel_sql("1 day", "", STEPS);
        assert_eq!(sql.matches('?').count(), 3);
    }

    #[test]
    fn test_report_conversion_handles_zero_first_step() {
        let report = to_report(&[(1, 0), (2, 0)]);
        assert!(report.iter().all(|r| r.conversion_rate == 0.0));
        assert!(report.iter().all(|r| r.dropped_off == 0));
    }

    #[test]
    fn test_report_dropoff_never_underflows() {
        // Defensive: a non-monotonic input must not wrap the unsigned subtraction.
        let report = to_report(&[(1, 5), (2, 9)]);
        assert_eq!(report[1].dropped_off, 0);
    }
}
