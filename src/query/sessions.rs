//! The one definition of a session, shared by every query that reports on them.
//!
//! Session metrics (`/stats/main`, `/stats/sessions`) and the entry/exit page
//! breakdowns used to carry their own copies of the `sessionize` SQL, and the
//! copies disagreed: one sessionised every event, the other only pageviews, so
//! the two endpoints reported different session counts for the same site. They
//! also applied filters *before* sessionising, which cut visits into pieces.
//! Everything now builds on [`per_session_cte`].
//!
//! # Definitions
//!
//! These follow the conventions shared by Plausible and Matomo, where one
//! exists, so a figure here means what a reader coming from either expects.
//!
//! - **Session**: a maximal run of one visitor's events, of any kind, in which
//!   no two consecutive events are further apart than the configured session
//!   window (30 minutes by default). Custom events keep a session alive: a
//!   visitor who watches a video for 25 minutes between two pageviews was never
//!   idle.
//! - **Counted session**: a session containing at least one pageview. An event
//!   with no pageview behind it in the range (a tracked click whose pageview
//!   predates the range) is not a visit, and counting it would drag every
//!   per-visit average toward zero.
//! - **Bounce**: a counted session with exactly one event, which is a pageview.
//!   A custom event is an interaction, so a visit with one pageview and one
//!   click is engaged. (Matomo counts events as actions; Plausible ends the
//!   bounce on an interactive event.)
//! - **Duration**: last event minus first event in the session, in seconds.
//! - **Views per visit**: mean pageviews per counted session.
//! - **Entry / exit page**: the pathname of the session's first / last
//!   pageview.
//! - **Filters select sessions, they do not cut them.** A session is kept when
//!   at least one of its events matches *every* filter, and it is then measured
//!   in full. Filtering the rows first made a two-page visit narrowed to one of
//!   its pages look like a one-page bounce.
//!
//! # Precision
//!
//! All of this assumes event timestamps are real. With `gdpr_mode` they are
//! rounded to the hour, and every figure here degenerates (see
//! `StatsParams` handling in `api::stats`); callers must not run these
//! queries on rounded data.

use super::QueryScope;

/// Window used when the configured one fails validation.
const FALLBACK_WINDOW: &str = "30 minutes";

/// The validated session window, interpolated into SQL.
///
/// It comes from operator configuration rather than request input and is
/// validated at startup; it is re-checked here so a future caller cannot
/// interpolate something arbitrary.
fn window(scope: &QueryScope) -> &str {
    if scope.session_window_is_safe() {
        scope.session_window.as_str()
    } else {
        FALLBACK_WINDOW
    }
}

/// Event names the optional extras script sends in the background, with no
/// action by the visitor: reaching a scroll depth and the time a page was
/// engaged with are both reported as the page is hidden.
///
/// They are real events (they count in `total_events` and can be queried), but
/// they are not interaction, so they must not end a bounce: turning the extras
/// on would otherwise make every visit look engaged. `tracking/extras.js` is
/// the source of these names; a test keeps the two in step.
pub const PASSIVE_EVENTS: &[&str] = &["Engagement", "Scroll Depth"];

/// `PASSIVE_EVENTS` as the body of a SQL `IN (...)`. The names are constants of
/// this crate, never request input, so inlining them is safe.
fn passive_sql_list() -> String {
    PASSIVE_EVENTS
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// SQL beginning a `WITH` list with a `per_session` CTE, one row per counted
/// session in the segment.
///
/// `per_session` has: `visitor_id`, `session_id`, `event_count`,
/// `interaction_count` (events other than [`PASSIVE_EVENTS`]), `page_count`,
/// `duration_secs`, `entry_page`, `exit_page`. The caller supplies the
/// statement that follows, e.g. `SELECT COUNT(*) FROM per_session`.
///
/// Bind with [`per_session_params`]: the filter values come first because the
/// segment expression sits in the `SELECT` list, ahead of the `WHERE`.
pub fn per_session_cte(scope: &QueryScope) -> String {
    let events = scope.events();
    format!(
        "WITH scoped AS (
             SELECT visitor_id, timestamp, event_name, pathname,
                    {segment} AS in_segment
             FROM {events} WHERE {range}
         ),
         sessionized AS (
             SELECT *, sessionize(timestamp, INTERVAL '{window}') OVER (
                 PARTITION BY visitor_id ORDER BY timestamp) AS session_id
             FROM scoped
         ),
         per_session AS (
             SELECT visitor_id, session_id,
                    COUNT(*) AS event_count,
                    COUNT(*) FILTER (WHERE event_name NOT IN ({passive}))
                        AS interaction_count,
                    COUNT(*) FILTER (WHERE event_name = 'pageview') AS page_count,
                    EXTRACT(EPOCH FROM (MAX(timestamp) - MIN(timestamp))) AS duration_secs,
                    MIN_BY(pathname, timestamp) FILTER (WHERE event_name = 'pageview')
                        AS entry_page,
                    MAX_BY(pathname, timestamp) FILTER (WHERE event_name = 'pageview')
                        AS exit_page
             FROM sessionized
             GROUP BY visitor_id, session_id
             HAVING COUNT(*) FILTER (WHERE event_name = 'pageview') >= 1
                AND bool_or(in_segment)
         )",
        segment = scope.filter_expr(),
        range = QueryScope::range_clause(),
        window = window(scope),
        passive = passive_sql_list(),
    )
}

/// The parameters of [`per_session_cte`], in order: filter values, then site,
/// start and end.
pub fn per_session_params(scope: &QueryScope) -> Vec<&str> {
    let mut out: Vec<&str> = scope.filter_params().collect();
    out.extend(scope.range_params());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{filtered_scope, scope};

    #[test]
    fn test_cte_binds_filter_values_before_the_range() {
        let s = filtered_scope("2024-01-01", "2024-02-01", "browsers==Chrome");
        let sql = per_session_cte(&s);
        assert_eq!(
            sql.matches('?').count(),
            per_session_params(&s).len(),
            "every ? needs exactly one value"
        );
        assert_eq!(
            per_session_params(&s),
            ["Chrome", "test.com", "2024-01-01", "2024-02-01"]
        );
        let segment_at = sql.find("browser = ?").expect("segment predicate");
        let range_at = sql.find("site_id = ?").expect("range predicate");
        assert!(segment_at < range_at, "bind order follows text order");
    }

    #[test]
    fn test_unfiltered_segment_is_a_constant() {
        let sql = per_session_cte(&scope("2024-01-01", "2024-02-01"));
        assert!(sql.contains("TRUE AS in_segment"));
        assert_eq!(sql.matches('?').count(), 3);
    }

    #[test]
    fn test_filters_are_applied_to_sessions_not_to_the_rows_that_form_them() {
        // The structural half of the S-02 regression: the filter must live in
        // the segment expression, never in the row-level WHERE that precedes
        // `sessionize`, or a visit is cut down to its matching events.
        let s = filtered_scope("2024-01-01", "2024-02-01", "pages==/pricing");
        let sql = per_session_cte(&s);
        let where_at = sql.find("FROM events_all WHERE").expect("scan");
        let where_clause = &sql[where_at..sql.find("sessionized AS").expect("cte")];
        assert!(
            !where_clause.contains("pathname = ?"),
            "a filter in the scan's WHERE cuts sessions apart: {where_clause}"
        );
    }

    #[test]
    fn test_an_unsafe_window_falls_back_to_the_default() {
        let s = QueryScope::new("a", "b", "c", "30 minutes; DROP TABLE events");
        let sql = per_session_cte(&s);
        assert!(sql.contains("INTERVAL '30 minutes'"));
        assert!(!sql.contains("DROP"));
    }
}
