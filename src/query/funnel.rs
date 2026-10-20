use super::QueryScope;
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// Maximum funnel steps.
///
/// The behavioral extension accepts up to 32 boolean conditions, matching
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
/// Returns `None` if any mode is unrecognised, or if `allow_reentry` is given
/// without `strict_order`: the extension rejects that combination at query time
/// (`behavioral` 0.10.0), which would otherwise surface as a 500 instead of a 400.
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
    let has = |m: &str| modes.iter().any(|x| x == m);
    if has("allow_reentry") && !has("strict_order") {
        return None;
    }
    Some(modes.join(", "))
}

/// Add `strict_once` unless the caller already chose a mode that gives each
/// event a single step.
///
/// `behavioral` 0.10.0 follows ClickHouse: one event that satisfies several
/// steps fills all of them. Steps built from `page:` and `event:` specifications
/// overlap routinely (a `pageview` step followed by a `page:/` step), so without
/// this a single pageview would count as having completed both. `strict_once`
/// and `strict_increase` restore one event, one step.
fn effective_modes(modes: &str) -> String {
    let one_event_per_step = modes
        .split(',')
        .map(str::trim)
        .any(|m| m == "strict_once" || m == "strict_increase");
    if one_event_per_step {
        modes.to_string()
    } else if modes.trim().is_empty() {
        "strict_once".to_string()
    } else {
        format!("{modes}, strict_once")
    }
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

    let sql = funnel_sql(scope, window_interval, modes, steps);
    let mut stmt = conn.prepare(&sql)?;
    let by_furthest: Vec<(usize, u64)> = stmt
        .query_map(duckdb::params_from_iter(scope.params()), |row| {
            Ok((row.get::<_, usize>(0)?, row.get::<_, u64>(1)?))
        })?
        .collect::<Result<_, _>>()?;

    Ok(to_report(&cumulative(&by_furthest, steps.len())?))
}

/// Turn "visitors whose furthest step was N" into "visitors who reached N".
///
/// `window_funnel` returns, per visitor, the furthest step reached (0 when not
/// even the first). A visitor who got to step 3 also got to steps 1 and 2, so
/// the count at step `k` is the sum over every `furthest >= k`. Doing this here
/// rather than in SQL keeps the statement at one row per distinct `furthest`;
/// the SQL version joined a step list to every visitor, materialising
/// `steps x visitors` rows (up to 32 x 1M) to produce `steps` numbers.
///
/// # Errors
///
/// A `furthest` beyond the number of steps cannot come from the extension, and
/// is reported rather than silently ignored.
fn cumulative(
    by_furthest: &[(usize, u64)],
    steps: usize,
) -> Result<Vec<(u32, u64)>, duckdb::Error> {
    let mut exact = vec![0u64; steps + 1];
    for &(furthest, visitors) in by_furthest {
        let slot = exact.get_mut(furthest).ok_or_else(|| {
            duckdb::Error::IntegralValueOutOfRange(0, i128::try_from(furthest).unwrap_or(i128::MAX))
        })?;
        *slot += visitors;
    }
    let mut out = Vec::with_capacity(steps);
    let mut reached = 0u64;
    for step in (1..=steps).rev() {
        reached += exact[step];
        let label = u32::try_from(step).map_err(|_| {
            duckdb::Error::IntegralValueOutOfRange(0, i128::try_from(step).unwrap_or(i128::MAX))
        })?;
        out.push((label, reached));
    }
    out.reverse();
    Ok(out)
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
fn funnel_sql(scope: &QueryScope, window_interval: &str, modes: &str, steps: &[&str]) -> String {
    let step_conditions = steps.join(", ");
    // `window_funnel(window [, mode], timestamp, cond, ...)`: the mode argument
    // is positional. `effective_modes` is never empty, so it is always present.
    let mode_arg = format!("'{}', ", effective_modes(modes).replace('\'', "''"));

    // One row per distinct "furthest step reached"; `cumulative` does the rest.
    format!(
        "SELECT furthest, COUNT(*) AS visitors
         FROM (
             SELECT visitor_id,
                 window_funnel(INTERVAL '{window_interval}', {mode_arg}timestamp,
                     {step_conditions}
                 ) AS furthest
             FROM events_all
             WHERE {where_clause}
             GROUP BY visitor_id
         )
         GROUP BY furthest",
        where_clause = scope.where_clause()
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
    fn test_filters_narrow_the_funnel() {
        let db = TestDb::new();
        if !db.require_behavioral("filtered funnel") {
            return;
        }
        // Two Chrome visitors reach step 2; the Firefox visitor only step 1.
        for (v, browser, steps) in [
            ("c1", "Chrome", vec!["/", "/pricing"]),
            ("c2", "Chrome", vec!["/", "/pricing"]),
            ("f1", "Firefox", vec!["/"]),
        ] {
            for (i, path) in steps.iter().enumerate() {
                db.insert_dimensional(
                    v,
                    &format!("2024-01-15 10:0{i}:00"),
                    path,
                    Some(browser),
                    None,
                    None,
                    None,
                );
            }
        }
        let conds = ["pathname = '/'", "pathname = '/pricing'"];

        let all = query_funnel(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "1 day",
            "",
            &conds,
        )
        .unwrap();
        assert_eq!(all[0].visitors, 3);
        assert_eq!(all[1].visitors, 2);

        let chrome = query_funnel(
            &db.conn,
            &crate::query::test_support::filtered_scope(
                "2024-01-01",
                "2024-02-01",
                "browsers==Chrome",
            ),
            "1 day",
            "",
            &conds,
        )
        .unwrap();
        assert_eq!(chrome[0].visitors, 2, "the Firefox visitor is excluded");
        assert_eq!(chrome[1].visitors, 2);
    }

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
        // "direct" goes straight from step 1 to step 2. "detour" views a page
        // that matches no step in between, which strict_order treats as a break.
        insert_pageview(&db.conn, "direct", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "direct", "2024-01-15 10:01:00", "/pricing");
        insert_pageview(&db.conn, "detour", "2024-01-15 11:00:00", "/");
        insert_pageview(&db.conn, "detour", "2024-01-15 11:01:00", "/about");
        insert_pageview(&db.conn, "detour", "2024-01-15 11:02:00", "/pricing");
        let two_steps = ["pathname = '/'", "pathname = '/pricing'"];
        let s = scope("2024-01-01", "2024-02-01");

        let relaxed = query_funnel(&db.conn, &s, "1 day", "", &two_steps).unwrap();
        assert_eq!(
            relaxed[1].visitors, 2,
            "unrelated pages do not break a funnel"
        );

        let strict = query_funnel(&db.conn, &s, "1 day", "strict_order", &two_steps).unwrap();
        assert_eq!(strict[0].visitors, 2);
        assert_eq!(
            strict[1].visitors, 1,
            "strict_order must break the detour's chain"
        );
    }

    #[test]
    fn test_one_event_fills_only_one_step_by_default() {
        let db = TestDb::new();
        if !db.require_behavioral("overlapping steps") {
            return;
        }
        // One pageview at "/" satisfies both `event_name = 'pageview'` and
        // `pathname = '/'`. Left to the extension's ClickHouse semantics it would
        // fill both steps and report a completed funnel from a single event.
        insert_pageview(&db.conn, "once", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "twice", "2024-01-15 11:00:00", "/");
        insert_pageview(&db.conn, "twice", "2024-01-15 11:01:00", "/");
        let overlapping = ["event_name = 'pageview'", "pathname = '/'"];
        let s = scope("2024-01-01", "2024-02-01");

        let rows = query_funnel(&db.conn, &s, "1 day", "", &overlapping).unwrap();
        assert_eq!(rows[0].visitors, 2);
        assert_eq!(
            rows[1].visitors, 1,
            "only the visitor with two events converts"
        );
    }

    #[test]
    fn test_strict_order_and_allow_reentry_run_together() {
        let db = TestDb::new();
        if !db.require_behavioral("funnel mode combinations") {
            return;
        }
        seed(&db);
        let rows = query_funnel(
            &db.conn,
            &scope("2024-01-01", "2024-02-01"),
            "1 day",
            "strict_order, allow_reentry",
            STEPS,
        )
        .expect("a mode combination normalize_modes accepts must execute");
        assert_eq!(rows.len(), 3);
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
        assert!(
            normalize_modes("allow_reentry").is_none(),
            "the extension rejects allow_reentry without strict_order"
        );
        assert_eq!(
            normalize_modes("allow_reentry, strict_order"),
            Some("allow_reentry, strict_order".to_string())
        );
        assert!(normalize_modes("drop_tables").is_none());
        assert!(normalize_modes("strict_order; DROP TABLE events").is_none());
    }

    #[test]
    fn test_effective_modes_adds_strict_once_unless_the_caller_chose_one() {
        assert_eq!(effective_modes(""), "strict_once");
        assert_eq!(effective_modes("strict_order"), "strict_order, strict_once");
        assert_eq!(effective_modes("strict_once"), "strict_once");
        assert_eq!(
            effective_modes("strict_order, strict_increase"),
            "strict_order, strict_increase"
        );
    }

    #[test]
    fn test_sql_always_passes_a_mode_argument() {
        // window_funnel's mode is positional, and the default has to be explicit
        // because the extension's own default lets one event fill several steps.
        let sql = funnel_sql(&scope("2024-01-01", "2024-02-01"), "1 day", "", STEPS);
        assert!(sql.contains("window_funnel(INTERVAL '1 day', 'strict_once', timestamp,"));
        let with_mode = funnel_sql(
            &scope("2024-01-01", "2024-02-01"),
            "1 day",
            "strict_order",
            STEPS,
        );
        assert!(
            with_mode.contains(
                "window_funnel(INTERVAL '1 day', 'strict_order, strict_once', timestamp,"
            )
        );
    }

    #[test]
    fn test_sql_uses_bound_parameters_for_scope() {
        let sql = funnel_sql(&scope("2024-01-01", "2024-02-01"), "1 day", "", STEPS);
        assert_eq!(sql.matches('?').count(), 3);
    }

    #[test]
    fn test_sql_does_not_join_steps_to_every_visitor() {
        // Regression (S-07): the final SELECT was `step_numbers LEFT JOIN
        // per_visitor ON TRUE`, a cross join that built steps x visitors rows.
        let sql = funnel_sql(&scope("2024-01-01", "2024-02-01"), "1 day", "", STEPS);
        assert!(!sql.contains("JOIN"), "{sql}");
        assert!(!sql.contains("generate_series"), "{sql}");
    }

    #[test]
    fn test_cumulative_sums_from_the_furthest_step_down() {
        // 1 visitor reached nothing, 2 stopped at step 1, 3 at step 2, 4 at 3.
        let rows = cumulative(&[(0, 1), (1, 2), (2, 3), (3, 4)], 3).unwrap();
        assert_eq!(rows, vec![(1, 9), (2, 7), (3, 4)]);
    }

    #[test]
    fn test_cumulative_keeps_a_row_for_a_step_nobody_reached() {
        let rows = cumulative(&[(1, 5)], 3).unwrap();
        assert_eq!(rows, vec![(1, 5), (2, 0), (3, 0)]);
        assert_eq!(cumulative(&[], 2).unwrap(), vec![(1, 0), (2, 0)]);
    }

    #[test]
    fn test_cumulative_rejects_a_furthest_step_beyond_the_funnel() {
        assert!(cumulative(&[(4, 1)], 3).is_err());
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
