use super::QueryScope;
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
    /// Fraction of sessions with exactly one pageview, 0.0–1.0.
    pub bounce_rate: Option<f64>,
    /// Mean session duration in seconds.
    pub avg_visit_duration_secs: Option<f64>,
    /// Mean pageviews per session.
    pub views_per_visit: Option<f64>,
    /// Whether session-derived fields above could be computed.
    pub behavioral_available: bool,
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
/// # Errors
///
/// Returns an error if the base counts cannot be read. A failure of the
/// session pass is not an error: it means the behavioral extension is
/// unavailable, and the affected fields are reported as `None`.
pub fn query_core_metrics(
    conn: &Connection,
    scope: &QueryScope,
) -> Result<CoreMetrics, duckdb::Error> {
    let base = query_base_counts(conn, scope)?;
    let sessions = query_session_aggregates(conn, scope).ok();

    #[allow(clippy::cast_precision_loss)]
    let views_per_visitor = if base.unique_visitors > 0 {
        base.total_pageviews as f64 / base.unique_visitors as f64
    } else {
        0.0
    };

    Ok(CoreMetrics {
        unique_visitors: base.unique_visitors,
        total_pageviews: base.total_pageviews,
        total_events: base.total_events,
        views_per_visitor,
        total_sessions: sessions.as_ref().map(|s| s.total_sessions),
        bounce_rate: sessions.as_ref().map(|s| s.bounce_rate),
        avg_visit_duration_secs: sessions.as_ref().map(|s| s.avg_duration_secs),
        views_per_visit: sessions.as_ref().map(|s| s.avg_pages),
        behavioral_available: sessions.is_some(),
    })
}

/// Visitor, pageview and event counts in one scan.
fn query_base_counts(conn: &Connection, scope: &QueryScope) -> Result<BaseCounts, duckdb::Error> {
    let sql = format!(
        "SELECT COUNT(DISTINCT visitor_id),
                COUNT(*) FILTER (WHERE event_name = 'pageview'),
                COUNT(*)
         FROM events_all WHERE {}",
        QueryScope::where_clause()
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
    stmt.query_row(duckdb::params_from_iter(scope.params()), |row| {
        Ok(SessionAggregates {
            total_sessions: row.get(0)?,
            bounce_rate: row.get(1)?,
            avg_duration_secs: row.get(2)?,
            avg_pages: row.get(3)?,
        })
    })
}

/// SQL for the session aggregate pass. Split out so it can be unit-tested.
fn session_aggregate_sql(scope: &QueryScope) -> String {
    // The session window comes from operator config, not request input, and is
    // validated by `Config::validate`; it is re-checked here so a future caller
    // cannot interpolate something arbitrary.
    let window = if scope.session_window_is_safe() {
        scope.session_window.clone()
    } else {
        "30 minutes".to_string()
    };
    format!(
        "WITH scoped AS (
             SELECT visitor_id, timestamp, event_name
             FROM events_all WHERE {where_clause}
         ),
         sessionized AS (
             SELECT visitor_id, timestamp, event_name,
                    sessionize(timestamp, INTERVAL '{window}') OVER (
                        PARTITION BY visitor_id ORDER BY timestamp) AS session_id
             FROM scoped
         ),
         per_session AS (
             SELECT visitor_id, session_id,
                    COUNT(*) FILTER (WHERE event_name = 'pageview') AS page_count,
                    EXTRACT(EPOCH FROM (MAX(timestamp) - MIN(timestamp))) AS duration_secs
             FROM sessionized GROUP BY visitor_id, session_id
         )
         SELECT COUNT(*),
                COALESCE(COUNT(*) FILTER (WHERE page_count = 1)::DOUBLE
                         / NULLIF(COUNT(*), 0), 0.0),
                COALESCE(AVG(duration_secs), 0.0),
                COALESCE(AVG(page_count), 0.0)
         FROM per_session",
        where_clause = QueryScope::where_clause()
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
