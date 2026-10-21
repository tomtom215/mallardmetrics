use super::{QueryScope, sessions};
use duckdb::Connection;
use serde::{Deserialize, Serialize};

/// Core metrics for a site over a time range.
///
/// Fields that depend on the `behavioral` extension are `Option`: `None` means
/// "could not be computed", which is meaningfully different from `0`. The
/// previous release reported a flat `0.0` bounce rate and `0.0` visit duration
/// whenever the extension was missing, which is indistinguishable from a site
/// where every visitor bounces instantly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreMetrics {
    /// Distinct `visitor_id` values in range.
    ///
    /// Note that `visitor_id` rotates with the configured salt period (daily by
    /// default), so over a range longer than one rotation this counts
    /// visitor-periods rather than people. See `visitor_salt_rotation_days`.
    pub unique_visitors: u64,
    /// Events with `event_name = 'pageview'`.
    pub total_pageviews: u64,
    /// All events, including custom ones.
    pub total_events: u64,
    /// `total_pageviews / unique_visitors`.
    ///
    /// Renamed from `pages_per_visit`, which was a misnomer: it was never
    /// per-visit, only ever per-visitor. `views_per_visit` below is the real
    /// per-session figure.
    pub views_per_visitor: f64,

    /// Sessions derived by `sessionize`. Requires the behavioral extension.
    pub total_sessions: Option<u64>,
    /// Fraction of sessions that were a single pageview and nothing else, 0.0–1.0.
    pub bounce_rate: Option<f64>,
    /// Mean session duration in seconds.
    pub avg_visit_duration_secs: Option<f64>,
    /// Mean pageviews per session.
    pub views_per_visit: Option<f64>,
    /// Whether session-derived fields above could be computed.
    pub behavioral_available: bool,
    /// Why they could not: `"timestamps_rounded"` (a privacy setting; the fix is
    /// to turn `round_timestamps`/`gdpr_mode` off) or `"extension_missing"`
    /// (the `behavioral` extension is not loaded). Absent when they could.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions_unavailable_reason: Option<String>,

    /// The same site over an earlier window, when the caller asked to compare.
    ///
    /// Absent from the response entirely when no comparison was requested, so
    /// a client that does not know about the field sees exactly what it did
    /// before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<ComparisonWindow>,
}

/// Headline figures for the window a report is being compared against.
///
/// Deliberately flat and deliberately a subset: a comparison exists to answer
/// "up or down, and by how much", and every field here is one a dashboard can
/// render a delta for. It carries its own dates because "the previous period"
/// is computed server-side, and a reader needs to know which days that was.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonWindow {
    /// Inclusive first day of the comparison window, `YYYY-MM-DD`.
    pub start_date: String,
    /// Inclusive last day of the comparison window, `YYYY-MM-DD`.
    pub end_date: String,
    pub unique_visitors: u64,
    pub total_pageviews: u64,
    pub total_events: u64,
    /// `None` when the behavioral extension could not compute it, exactly as
    /// in [`CoreMetrics`].
    pub total_sessions: Option<u64>,
    pub bounce_rate: Option<f64>,
    pub avg_visit_duration_secs: Option<f64>,
}

impl ComparisonWindow {
    /// Build a comparison window from a scope's metrics and its dates.
    pub const fn from_metrics(start_date: String, end_date: String, metrics: &CoreMetrics) -> Self {
        Self {
            start_date,
            end_date,
            unique_visitors: metrics.unique_visitors,
            total_pageviews: metrics.total_pageviews,
            total_events: metrics.total_events,
            total_sessions: metrics.total_sessions,
            bounce_rate: metrics.bounce_rate,
            avg_visit_duration_secs: metrics.avg_visit_duration_secs,
        }
    }
}

/// Counts that do not need the behavioral extension.
struct BaseCounts {
    unique_visitors: u64,
    total_pageviews: u64,
    total_events: u64,
}

/// Session-derived metrics.
struct SessionAggregates {
    total_sessions: u64,
    bounce_rate: f64,
    avg_duration_secs: f64,
    avg_pages: f64,
}

/// Query all core metrics for a scope.
///
/// Runs at most two statements: one scan for the plain counts and one
/// `sessionize` pass for the session-derived figures. The previous
/// implementation issued four independent full scans of `events_all` — the
/// counts, the bounce rate, and the session metrics each re-read the range,
/// and the session query ran twice because `/stats/main` and `/stats/sessions`
/// both called it.
///
/// Counts (`unique_visitors`, `total_pageviews`, `total_events`) are
/// *event-level*: they count the events that match the scope's filters. The
/// session figures are *session-level*: under a filter they describe the whole
/// of every session that contains a matching event. See
/// [`crate::query::sessions`] for the definitions.
///
/// # Errors
///
/// Returns an error if the base counts cannot be read, or if the session pass
/// fails for any reason other than the behavioral extension being absent. Only
/// absence reports the session fields as `None`: an out-of-memory, an I/O
/// error, a timeout or a bad Parquet file used to be reported the same way and
/// the degraded body was then cached, so a transient failure read as "this
/// deployment has no session data" for a whole TTL.
pub fn query_core_metrics(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<CoreMetrics, duckdb::Error> {
    let base = query_base_counts(conn, scope)?;
    let sessions = match query_session_aggregates(conn, scope) {
        Ok(aggregates) => Some(aggregates),
        Err(e) if super::behavioral_loaded(conn) => return Err(e),
        Err(_) => None,
    };
    Ok(assemble(&base, sessions.as_ref()))
}

/// The same metrics without the session pass.
///
/// For deployments whose timestamps are rounded (`gdpr_mode`): every session
/// figure computed from hour-rounded times is an artefact of the rounding, so
/// they are reported as unavailable rather than as plausible-looking numbers.
///
/// # Errors
///
/// Returns an error if the base counts cannot be read.
pub fn query_event_metrics(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<CoreMetrics, duckdb::Error> {
    Ok(assemble(&query_base_counts(conn, scope)?, None))
}

fn assemble(base: &BaseCounts, sessions: Option<&SessionAggregates>) -> CoreMetrics {
    #[allow(clippy::cast_precision_loss)]
    let views_per_visitor = if base.unique_visitors > 0 {
        base.total_pageviews as f64 / base.unique_visitors as f64
    } else {
        0.0
    };

    CoreMetrics {
        unique_visitors: base.unique_visitors,
        total_pageviews: base.total_pageviews,
        total_events: base.total_events,
        views_per_visitor,
        total_sessions: sessions.map(|s| s.total_sessions),
        bounce_rate: sessions.map(|s| s.bounce_rate),
        avg_visit_duration_secs: sessions.map(|s| s.avg_duration_secs),
        views_per_visit: sessions.map(|s| s.avg_pages),
        behavioral_available: sessions.is_some(),
        // The caller knows why sessions were skipped; it fills this in.
        sessions_unavailable_reason: None,
        comparison: None,
    }
}

/// Visitor, pageview and event counts in one scan.
fn query_base_counts(conn: &Connection, scope: &QueryScope) -> Result<BaseCounts, duckdb::Error> {
    let events = scope.events();
    let sql = format!(
        "SELECT COUNT(DISTINCT visitor_id),
                COUNT(*) FILTER (WHERE event_name = 'pageview'),
                COUNT(*)
         FROM {events} WHERE {}",
        scope.where_clause()
    );
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(duckdb::params_from_iter(scope.params()), |row| {
        Ok(BaseCounts {
            unique_visitors: row.get(0)?,
            total_pageviews: row.get(1)?,
            total_events: row.get(2)?,
        })
    })
}

/// Session count, bounce rate, mean duration and mean pages in one `sessionize` pass.
fn query_session_aggregates(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<SessionAggregates, duckdb::Error> {
    let sql = session_aggregate_sql(scope);
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(
        duckdb::params_from_iter(sessions::per_session_params(scope)),
        |row| {
            Ok(SessionAggregates {
                total_sessions: row.get(0)?,
                bounce_rate: row.get(1)?,
                avg_duration_secs: row.get(2)?,
                avg_pages: row.get(3)?,
            })
        },
    )
}

/// SQL for the session aggregate pass. Split out so it can be unit-tested.
///
/// A bounce is a counted session with exactly one event, which is a pageview;
/// the full set of definitions is in [`crate::query::sessions`].
fn session_aggregate_sql(scope: &QueryScope) -> String {
    format!(
        "{cte}
         SELECT COUNT(*),
                COALESCE(COUNT(*) FILTER (WHERE event_count = 1 AND page_count = 1)::DOUBLE
                         / NULLIF(COUNT(*), 0), 0.0),
                COALESCE(AVG(duration_secs), 0.0),
                COALESCE(AVG(page_count), 0.0)
         FROM per_session",
        cte = sessions::per_session_cte(scope)
    )
}

/// Session-level metrics, exposed on its own endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub total_sessions: u64,
    pub avg_session_duration_secs: f64,
    pub avg_pages_per_session: f64,
    pub bounce_rate: f64,
}

/// Query session metrics.
///
/// # Errors
///
/// Returns an error when the behavioral extension is not loaded.
pub fn query_session_metrics(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<SessionMetrics, duckdb::Error> {
    let agg = query_session_aggregates(conn, scope)?;
    Ok(SessionMetrics {
        total_sessions: agg.total_sessions,
        avg_session_duration_secs: agg.avg_duration_secs,
        avg_pages_per_session: agg.avg_pages,
        bounce_rate: agg.bounce_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::test_support::{TestDb, insert_pageview, scope};

    #[test]
    fn test_unique_visitors_empty() {
        let db = TestDb::new();
        let m = query_core_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(m.unique_visitors, 0);
        assert_eq!(m.total_pageviews, 0);
    }

    #[test]
    fn test_counts() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:05:00", "/about");
        insert_pageview(&db.conn, "v2", "2024-01-15 11:00:00", "/");

        let m = query_core_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(m.unique_visitors, 2);
        assert_eq!(m.total_pageviews, 3);
        assert_eq!(m.total_events, 3);
        assert!((m.views_per_visitor - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_date_range_is_half_open() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "v2", "2024-02-15 10:00:00", "/");

        let m = query_core_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(m.unique_visitors, 1, "the end bound must be exclusive");
    }

    #[test]
    fn test_pageviews_exclude_custom_events() {
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        db.insert_event("v1", "2024-01-15 10:01:00", "signup", "/");

        let m = query_core_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(m.total_pageviews, 1);
        assert_eq!(m.total_events, 2, "custom events count toward total_events");
    }

    #[test]
    fn test_behavioral_fields_are_none_without_the_extension() {
        // Reporting 0.0 was indistinguishable from "every visitor bounced".
        let db = TestDb::new();
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        let m = query_core_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        if !db.behavioral {
            assert!(m.bounce_rate.is_none());
            assert!(m.avg_visit_duration_secs.is_none());
            assert!(m.total_sessions.is_none());
            assert!(!m.behavioral_available);
        }
    }

    #[test]
    fn test_session_metrics_with_behavioral_extension() {
        let db = TestDb::new();
        if !db.require_behavioral("session metrics") {
            return;
        }
        // v1: two pageviews five minutes apart -> one session, not a bounce.
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:05:00", "/about");
        // v2: a single pageview -> one session, a bounce.
        insert_pageview(&db.conn, "v2", "2024-01-15 11:00:00", "/");

        let m = query_core_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert!(m.behavioral_available);
        assert_eq!(m.total_sessions, Some(2));
        assert_eq!(m.bounce_rate, Some(0.5));
        assert_eq!(m.views_per_visit, Some(1.5));
        // v1's session spans 300s, v2's spans 0s -> mean 150s.
        assert_eq!(m.avg_visit_duration_secs, Some(150.0));
    }

    #[test]
    fn test_filters_reach_the_session_pass() {
        // `query_core_metrics` swallows a session-query failure into `None`, so
        // a filter that broke that SQL would blank the session fields rather
        // than erroring — and a test asserting only on pageviews would still
        // pass. This asserts the session figures themselves under a filter.
        let db = TestDb::new();
        if !db.require_behavioral("filtered session metrics") {
            return;
        }
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/",
            Some("Chrome"),
            None,
            None,
            None,
        );
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:05:00",
            "/about",
            Some("Chrome"),
            None,
            None,
            None,
        );
        db.insert_dimensional(
            "v2",
            "2024-01-15 11:00:00",
            "/",
            Some("Firefox"),
            None,
            None,
            None,
        );

        let all = query_core_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(all.total_sessions, Some(2));
        assert_eq!(all.bounce_rate, Some(0.5));

        let chrome = query_core_metrics(
            &db.conn,
            &crate::query::test_support::filtered_scope(
                "2024-01-01",
                "2024-02-01",
                "browsers==Chrome",
            ),
        )
        .unwrap();
        assert!(
            chrome.behavioral_available,
            "the filtered session pass failed and was silently reported as unavailable"
        );
        assert_eq!(chrome.total_sessions, Some(1));
        assert_eq!(
            chrome.bounce_rate,
            Some(0.0),
            "the one Chrome session has two pageviews"
        );
        assert_eq!(chrome.views_per_visit, Some(2.0));
        assert_eq!(chrome.total_pageviews, 2);
    }

    #[test]
    fn test_filtered_session_metrics_endpoint_query() {
        let db = TestDb::new();
        if !db.require_behavioral("filtered session metrics") {
            return;
        }
        db.insert_dimensional(
            "v1",
            "2024-01-15 10:00:00",
            "/",
            Some("Chrome"),
            None,
            None,
            None,
        );
        db.insert_dimensional(
            "v2",
            "2024-01-15 11:00:00",
            "/",
            Some("Firefox"),
            None,
            None,
            None,
        );

        let m = query_session_metrics(
            &db.conn,
            &crate::query::test_support::filtered_scope(
                "2024-01-01",
                "2024-02-01",
                "browsers==Firefox",
            ),
        )
        .unwrap();
        assert_eq!(m.total_sessions, 1);
    }

    /// The audit's scenario: `c` views `/home` then `/pricing` in one visit,
    /// `d` views `/x` and leaves.
    fn seed_two_page_visit(db: &TestDb) {
        insert_pageview(&db.conn, "c", "2024-01-15 10:00:00", "/home");
        insert_pageview(&db.conn, "c", "2024-01-15 10:05:00", "/pricing");
        insert_pageview(&db.conn, "d", "2024-01-15 11:00:00", "/x");
    }

    #[test]
    fn test_page_filter_keeps_whole_sessions_not_just_the_matching_events() {
        // Regression (S-02): sessions were rebuilt from the filtered rows, so a
        // two-page visit narrowed to its `/pricing` pageview looked like a
        // one-page visit and was reported as a bounce lasting 0 seconds.
        // Plausible and Matomo both treat a segment as "visits containing a
        // matching action" and measure the visit in full.
        let db = TestDb::new();
        if !db.require_behavioral("filtered session metrics") {
            return;
        }
        seed_two_page_visit(&db);
        let m = query_core_metrics(
            &db.conn,
            &crate::query::test_support::filtered_scope(
                "2024-01-01",
                "2024-02-01",
                "pages==/pricing",
            ),
        )
        .unwrap();
        assert_eq!(m.total_sessions, Some(1), "only c's visit touched /pricing");
        assert_eq!(m.bounce_rate, Some(0.0), "c viewed two pages");
        assert_eq!(m.avg_visit_duration_secs, Some(300.0));
        assert_eq!(
            m.views_per_visit,
            Some(2.0),
            "the whole visit, not one view"
        );
        // Event-level counts stay event-level: one matching pageview.
        assert_eq!(m.total_pageviews, 1);
    }

    #[test]
    fn test_a_pageview_plus_a_custom_event_is_not_a_bounce() {
        // Regression (S-03): bounce was `pageviews = 1`, so a visitor who viewed
        // one page and then clicked a tracked button bounced. Both Plausible
        // (an interactive event ends the bounce) and Matomo (events are
        // actions) count that visit as engaged.
        let db = TestDb::new();
        if !db.require_behavioral("bounce definition") {
            return;
        }
        insert_pageview(&db.conn, "a", "2024-01-15 10:00:00", "/");
        db.insert_event("a", "2024-01-15 10:01:00", "signup", "/");
        insert_pageview(&db.conn, "b", "2024-01-15 11:00:00", "/");

        let m = query_session_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(m.total_sessions, 2);
        assert!((m.bounce_rate - 0.5).abs() < f64::EPSILON, "{m:?}");
        assert!(
            (m.avg_pages_per_session - 1.0).abs() < f64::EPSILON,
            "a custom event is not a page"
        );
        assert!(
            (m.avg_session_duration_secs - 30.0).abs() < f64::EPSILON,
            "duration spans every event in the visit: (60 + 0) / 2"
        );
    }

    #[test]
    fn test_a_visit_with_no_pageview_is_not_a_session() {
        // Regression (S-03): a custom event with no pageview in range (a
        // script firing after the range began, a pageview that predates it)
        // counted as a session, diluting views-per-visit and the bounce rate.
        let db = TestDb::new();
        if !db.require_behavioral("session definition") {
            return;
        }
        db.insert_event("only-event", "2024-01-15 10:00:00", "signup", "/");
        insert_pageview(&db.conn, "real", "2024-01-15 11:00:00", "/");

        let m = query_core_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(m.total_sessions, Some(1));
        assert_eq!(m.bounce_rate, Some(1.0));
        assert_eq!(m.views_per_visit, Some(1.0));
        assert_eq!(m.unique_visitors, 2, "visitor counts are event-level");
    }

    #[test]
    fn test_custom_events_keep_a_session_alive() {
        // A pageview, a custom event 20 minutes on, and a pageview 20 minutes
        // after that are one visit: the visitor was never idle for 30 minutes.
        let db = TestDb::new();
        if !db.require_behavioral("session definition") {
            return;
        }
        insert_pageview(&db.conn, "e", "2024-01-15 12:00:00", "/e1");
        db.insert_event("e", "2024-01-15 12:20:00", "signup", "/e1");
        insert_pageview(&db.conn, "e", "2024-01-15 12:40:00", "/e2");

        let m = query_session_metrics(&db.conn, &scope("2024-01-01", "2024-02-01")).unwrap();
        assert_eq!(m.total_sessions, 1);
        assert!((m.avg_session_duration_secs - 2400.0).abs() < f64::EPSILON);
        assert!((m.avg_pages_per_session - 2.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_a_failing_session_pass_is_an_error_not_missing_data() {
        // Regression (S-06): `query_session_aggregates(..).ok()` turned every
        // failure into `behavioral_available: false`, and the degraded body was
        // then cached. Subtracting an infinite timestamp is a genuine runtime
        // error that has nothing to do with the extension being absent.
        let db = TestDb::new();
        if !db.require_behavioral("error propagation") {
            return;
        }
        db.conn
            .execute(
                "INSERT INTO events (site_id, visitor_id, timestamp, event_name, pathname)
                 VALUES ('test.com', 'inf', CAST('-infinity' AS TIMESTAMP), 'pageview', '/'),
                        ('test.com', 'inf', CAST('2024-01-15 10:00:00' AS TIMESTAMP), 'pageview', '/')",
                [],
            )
            .unwrap();
        let everything = scope("-infinity", "2024-02-01");
        assert!(
            query_core_metrics(&db.conn, &everything).is_err(),
            "a real failure must reach the caller, not read as \"extension unavailable\""
        );
    }

    #[test]
    fn test_session_window_splits_sessions() {
        let db = TestDb::new();
        if !db.require_behavioral("session window") {
            return;
        }
        // Two pageviews 45 minutes apart: one session at a 60-minute window,
        // two at the 30-minute default.
        insert_pageview(&db.conn, "v1", "2024-01-15 10:00:00", "/");
        insert_pageview(&db.conn, "v1", "2024-01-15 10:45:00", "/about");

        let narrow = QueryScope::new("test.com", "2024-01-01", "2024-02-01", "30 minutes");
        let wide = QueryScope::new("test.com", "2024-01-01", "2024-02-01", "60 minutes");

        assert_eq!(
            query_session_metrics(&db.conn, &narrow)
                .unwrap()
                .total_sessions,
            2
        );
        assert_eq!(
            query_session_metrics(&db.conn, &wide)
                .unwrap()
                .total_sessions,
            1
        );
    }

    #[test]
    fn test_unsafe_session_window_falls_back_to_the_default() {
        let scope = QueryScope::new(
            "a.com",
            "2024-01-01",
            "2024-02-01",
            "30 minutes; DROP TABLE events",
        );
        let sql = session_aggregate_sql(&scope);
        assert!(sql.contains("INTERVAL '30 minutes'"));
        assert!(!sql.contains("DROP TABLE"));
    }

    #[test]
    fn test_session_sql_binds_scope_parameters() {
        let sql = session_aggregate_sql(&scope("2024-01-01", "2024-02-01"));
        assert_eq!(sql.matches('?').count(), 3);
        assert!(
            !sql.contains("test.com"),
            "site_id must be bound, not inlined"
        );
    }
}
